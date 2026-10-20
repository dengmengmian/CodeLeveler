import { renderMarkdown } from './markdown.mjs';
import {
  modelLabel, workspaceLabel, toolLabel, toolTarget, browserActivity, sourceErrorSummary,
  relativeActivity, permissionLabel, recentWorkspaces, renameCommand, messageKind,
  historyQuestions, planStepLabel, browserChrome, browserSurfaceBounds, workspaceTaskGroups,
  attachmentErrorPresentation, slashIntent, commandCandidates, groupExecutionRounds,
  roundGlyph, roundHeadline, contractToolStatus, failureReason, displayPreview,
  confirmedDiffOf, diffCounts, foldedThoughts, turnBlocks, thoughtDuration,
} from './presentation.mjs';
import {
  projectSnapshot, applySessionMetadata, applyEvent, commandEnvelope, terminalEvents,
  approvalDecisions, clarificationAnswer, uploadDraft, applyUploadEvent, reconcileUploadReply,
  workspaceDraftText, attachmentSubmissionError, permissionCommand, permissionConfirmed,
  beginDiffQuery, expireDiffQuery, retainInterruptedThoughts,
} from './state.mjs';
import { brandMark, fileIcon } from './icons.mjs';
import { projectTrajectory, searchTrajectory, buildTimeline, formatDuration, trajectoryWindow } from './trajectory.mjs';
import {
  el, iconNode, iconButton, installTooltips, toast, openMenu, closeMenu, confirmDialog,
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
let pendingCreation = null;
let runStartedAt = null;
let runClockTimer = null;
let paintPending = false;
let attachmentEarly = null;
let uploadingKey = null;
let titleEditing = false;
let titleComposing = false;
let findMatches = [];
let findSelected = 0;
let sidebarQuery = '';
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
let openInAppState = { apps: null, loading: false, error: null, preferred: null };

const drafts = new Map();
const attachmentDrafts = new Map();
const renderedSources = new WeakMap();
const openThoughts = new Set();
const openProcess = new Set();
const openTools = new Set();
const foldedGroups = new Set();

/* ------------------------------------------------------------ preferences */

const SETTINGS_KEY = 'codeleveler.settings.v1';
const settings = loadSettings();

function loadSettings() {
  const defaults = {
    theme: 'system', fontSize: 14, codeFontSize: 11, terminalFontSize: 13,
    textFontFamily: '', codeFontFamily: '', terminalFontFamily: '',
    workSteps: 'standard', collapseAfter: 'answer', linkOpen: 'side', busySend: 'queue',
    performance: 'detailed', codingView: true,
    defaultPermission: 'assisted', archivedFilter: 'default', groupMode: 'workspace',
    sortMode: 'updated',
  };
  try {
    const stored = JSON.parse(localStorage.getItem(SETTINGS_KEY) || '{}');
    const merged = { ...defaults, ...(stored && typeof stored === 'object' ? stored : {}) };
    if (merged.workSteps === 'minimal') merged.workSteps = 'compact';
    if (!['compact', 'standard', 'detailed', 'verbose'].includes(merged.workSteps)) merged.workSteps = 'standard';
    if (merged.collapseAfter === 'never') merged.collapseAfter = 'next';
    if (!['answer', 'next'].includes(merged.collapseAfter)) merged.collapseAfter = 'answer';
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
const VIEW_KEY = 'codeleveler.views.v1';
const OPEN_APP_KEY = 'codeleveler.open-in-app.v1';
function savedView(sessionId) {
  try {
    const stored = JSON.parse(localStorage.getItem(VIEW_KEY) || '{}');
    return stored && stored[sessionId] === 'trajectory' ? 'trajectory' : 'chat';
  } catch { return 'chat'; }
}
function rememberView(sessionId, view) {
  if (!sessionId) return;
  try {
    const stored = JSON.parse(localStorage.getItem(VIEW_KEY) || '{}');
    stored[sessionId] = view;
    localStorage.setItem(VIEW_KEY, JSON.stringify(stored));
  } catch { /* optional */ }
}
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
function stashDraft() { drafts.set(draftKey(), $('message').value); }

function controls() {
  const uploads = uploadPending();
  const hasContent = $('message').value.trim() !== '' || attachments().some(item => item.status === 'ready');
  const running = ['running', 'waiting_user'].includes(current?.status);
  const frozen = loading || !!uncertain || uploads;
  $('send').disabled = frozen || !hasContent;
  $('attachment-add').disabled = frozen;
  $('model-picker').disabled = frozen;
  $('permission-picker').disabled = frozen;
  $('workspace-picker').disabled = frozen;
  $('new-session').disabled = loading || uploads;
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

function sortedTasks() {
  const mode = settings.sortMode ?? 'updated';
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

function renderSidebar() {
  const list = $('workspace-list');
  list.replaceChildren();
  const visible = visibleTasks();
  const groups = [];
  if ((settings.groupMode ?? 'workspace') === 'flat') {
    groups.push({ path: null, name: '全部会话', tasks: visible, flat: true });
  } else {
    const grouped = workspaceTaskGroups(visible);
    if (grouped.loose.length) groups.push({ path: null, name: '无工作区', tasks: grouped.loose });
    for (const space of grouped.spaces) groups.push({ path: space.path, name: space.name, tasks: space.tasks });
  }
  if (!visible.length) list.append(el('p', 'workspace-empty', sidebarQuery ? '没有匹配的会话' : '暂无会话'));
  for (const group of groups) {
    const section = el('section', 'workspace-group');
    if (group.path) section.dataset.workspacePath = group.path;
    const key = group.path ?? '(loose)';
    const collapsed = foldedGroups.has(key);
    const head = el('div', 'workspace-group-head');
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
            { label: '重命名工作区', icon: 'edit', disabled: true, badge: '未接入' },
            { label: '删除工作区', icon: 'trash', disabled: true, badge: '未接入' },
          ],
        });
      });
      head.append(create, more);
    }
    section.append(head);
    if (!collapsed) {
      const sessions = el('div', 'workspace-sessions');
      for (const task of group.tasks) sessions.append(sessionRow(task));
      if (!group.tasks.length) sessions.append(el('p', 'workspace-empty', '暂无会话'));
      section.append(sessions);
    }
    list.append(section);
  }
}

function toggleGroup(key) {
  if (foldedGroups.has(key)) foldedGroups.delete(key); else foldedGroups.add(key);
  renderSidebar();
}

function sessionRow(task) {
  const row = el('div', 'session-row');
  row.setAttribute('role', 'treeitem');
  row.dataset.sessionId = task.id;
  row.dataset.status = task.status ?? 'unknown';
  row.setAttribute('aria-selected', String(task.id === current?.session.id));
  row.tabIndex = 0;
  const archived = task.archived === true || !!task.archived_at;
  const title = el('span', 'session-title', task.title || '未命名会话');
  if (archived) title.style.color = 'var(--text-caption)';
  const meta = el('span', 'session-meta', archived ? '已归档' : relativeActivity(task.last_activity_at ?? task.updated_at));
  const more = el('button', 'icon-button session-more');
  more.type = 'button';
  more.setAttribute('aria-label', `会话操作：${task.title || '未命名会话'}`);
  more.append(iconNode('more', { size: 16 }));
  more.addEventListener('click', event => { event.stopPropagation(); openSessionMenu(task, more); });
  row.append(title, meta, more);
  row.addEventListener('click', () => {
    if (loading || uncertain || uploadPending()) return;
    if (task.id === current?.session.id) { closeNarrowSidebar(); return; }
    action(() => openTask(task));
  });
  row.addEventListener('keydown', event => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    row.click();
  });
  row.addEventListener('contextmenu', event => { event.preventDefault(); openSessionMenu(task, more); });
  return row;
}

function openSessionMenu(task, anchor) {
  const archived = task.archived === true || !!task.archived_at;
  openMenu({
    anchor,
    width: 220,
    items: [
      { label: '置顶', icon: 'pin', disabled: true, badge: '未接入' },
      { label: '重命名', icon: 'edit', onSelect: () => action(async () => { await openTask(task); beginTitleEdit(); }) },
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
      { label: '登录', icon: 'user', disabled: true, title: '当前版本未接入登录' },
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
      { label: '平铺', checked: group === 'flat', onSelect: () => { settings.groupMode = 'flat'; saveSettings(); renderSidebar(); } },
      { heading: '排序' },
      { label: '最近更新', checked: sort === 'updated', onSelect: () => { settings.sortMode = 'updated'; saveSettings(); renderSidebar(); } },
      { label: '创建时间', checked: sort === 'created', onSelect: () => { settings.sortMode = 'created'; saveSettings(); renderSidebar(); } },
      { label: '名称', checked: sort === 'name', onSelect: () => { settings.sortMode = 'name'; saveSettings(); renderSidebar(); } },
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
  input.addEventListener('input', () => { sidebarQuery = input.value; renderSidebar(); });
  const close = () => { sidebarQuery = ''; input.replaceWith(label); renderSidebar(); };
  input.addEventListener('keydown', event => { if (event.key === 'Escape') { event.preventDefault(); close(); } });
  input.addEventListener('blur', () => { if (!input.value) close(); });
}

/* ---------------------------------------------------------------- header */

function ensureHeaderActions() {
  if (headerMenuButton) return;
  headerMenuButton = $('header-more');
  headerMenuButton.append(iconNode('more', { size: 16 }));
  headerMenuButton.addEventListener('click', () => {
    if (!current) return;
    openMenu({
      anchor: headerMenuButton, align: 'end', width: 240,
      items: [
        { label: '下载 Session 日志', icon: 'download', disabled: true, title: 'Runtime 未提供 Session 打包导出接口' },
        { label: '反馈', icon: 'feedback', onSelect: openFeedbackDialog },
      ],
    });
  });
  const preset = $('header-preset');
  if (preset && !preset.querySelector('svg')) preset.replaceChildren(iconNode('shield', { size: 14 }), el('span', undefined, '标准模式'));
  const actions = $('conversation-header').querySelector('.header-actions');
  headerJobsButton = iconButton('clock', '后台任务', { size: 16, tooltip: '后台任务' });
  headerSubagentButton = iconButton('bot', '子智能体', { size: 16, tooltip: '子智能体' });
  actions.insertBefore(headerSubagentButton, headerMenuButton);
  actions.insertBefore(headerJobsButton, headerMenuButton);
  headerSubagentButton.addEventListener('click', () => openRightbarTab('subagent'));
  headerJobsButton.addEventListener('click', event => openJobsMenu(event.currentTarget));
  for (const tab of document.querySelectorAll('#view-tabs [data-view]')) {
    tab.addEventListener('click', () => {
      activeView = tab.dataset.view === 'trajectory' ? 'trajectory' : 'chat';
      if (current) rememberView(current.session.id, activeView);
      renderHeader();
      renderConversation();
      renderTrajectory();
      controls();
    });
  }
}

function feedbackCategories() {
  return ['任务结果', '指令理解与遵循', '产品功能与交互', '稳定性和速度', '资源使用与费用', '安全隐私与权限', '其他'];
}

function openFeedbackDialog() {
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
  const selected = new Set();
  for (const category of feedbackCategories()) {
    const chip = el('button', 'feedback-chip', category);
    chip.type = 'button';
    chip.setAttribute('aria-pressed', 'false');
    chip.addEventListener('click', () => {
      if (selected.has(category)) selected.delete(category); else selected.add(category);
      chip.setAttribute('aria-pressed', String(selected.has(category)));
    });
    chips.append(chip);
  }
  const detail = el('textarea', 'feedback-detail');
  detail.rows = 5;
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
      tab.setAttribute('aria-selected', String(tab.dataset.view === activeView));
    }
  }
  if (!current) {
    if (!titleEditing) $('conversation-title').textContent = '新建会话';
    $('conversation-status').textContent = '';
    renderOpenInAppControls();
    return;
  }
  if (!titleEditing) $('conversation-title').textContent = current.session.goal || '新建会话';
  // The reference header carries no status sentence; runtime state is shown in
  // the transcript and the composer. Keep the element for assistive tech only.
  $('conversation-status').textContent = current.approvals.length || current.clarifications.length
    ? '等待你处理' : current.activity || LABELS[current.status] || current.status;
  $('conversation-status').dataset.tone = current.status === 'failed' ? 'error'
    : current.status === 'running' ? 'active' : current.approvals.length || current.clarifications.length ? 'waiting' : '';
  $('conversation-status').hidden = true;
  if (headerSubagentButton) headerSubagentButton.hidden = !(current.children?.length);
  if (headerJobsButton) headerJobsButton.hidden = !(current.backgroundTasks?.length);
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
  if (!path || !appId || openInAppState.busy) return;
  openInAppState.busy = true;
  renderOpenInAppControls();
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
  $('stop').replaceChildren(iconNode('stop', { size: 12 }));
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
  const panel = contextMeterPanel();
  host.hidden = !panel;
  host.replaceChildren(...(panel ? [panel] : []));
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
  renderContextMeter();
  renderConversation();
  renderTrajectory();
  renderInteractions();
  renderRightbar();
  controls();
  layout();
}

/* ---------------------------------------------------------- conversation */

function messageRow(existing, message) {
  const key = `message:${message.id}`;
  const kind = messageKind(message);
  let row = existing.get(key);
  if (!row) {
    if (kind === 'user') {
      row = el('article', 'message user');
      row.dataset.key = key;
      row.dataset.messageId = message.id;
      row.append(el('div', 'bubble'), el('div', 'message-actions'));
    } else if (kind === 'assistant') {
      row = el('article', 'message assistant');
      row.dataset.key = key;
      row.dataset.messageId = message.id;
      row.append(el('div', 'markdown'), el('div', 'message-actions'));
    } else {
      row = el('article', 'message notice');
      row.dataset.key = key;
      row.dataset.messageId = message.id;
      row.append(el('span', 'notice-pill'));
    }
  }
  if (kind === 'user') {
    const bubble = row.querySelector('.bubble');
    if (renderedSources.get(bubble) !== message.text) {
      renderedSources.set(bubble, message.text);
      bubble.textContent = message.text;
    }
    decorateMessageActions(row, message);
    const images = Number(message.images ?? 0);
    let imageRow = row.querySelector('.message-images');
    if (images > 0 && !imageRow) { imageRow = el('div', 'message-images', `图片 × ${images}`); row.querySelector('.bubble').after(imageRow); }
    if (images <= 0 && imageRow) imageRow.remove();
  } else if (kind === 'assistant') {
    const body = row.querySelector('.markdown');
    if (renderedSources.get(body) !== message.text) {
      renderedSources.set(body, message.text);
      renderMarkdown(body, message.text, { onLink: openMessageLink });
      addCodeActions(body);
    }
    decorateMessageActions(row, message);
  } else {
    row.querySelector('.notice-pill').textContent = message.text;
  }
  return row;
}

function decorateMessageActions(row, message) {
  let actions = row.querySelector('.message-actions');
  if (!actions) { actions = el('div', 'message-actions'); row.append(actions); }
  if (actions.dataset.for === message.id) return;
  actions.dataset.for = message.id;
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
  branch.setAttribute('aria-label', '分叉为新会话（未接入）');
  branch.setAttribute('aria-disabled', 'true');
  branch.dataset.tooltip = '分叉为新会话（未接入）';
  branch.append(iconNode('fork', { size: 15 }));
  branch.addEventListener('click', () => toast('分叉会话尚未接入', { tone: 'info', holdMs: 2200 }));
  actions.replaceChildren(copy, branch);
}

function addCodeActions(body) {
  for (const pre of body.querySelectorAll('pre')) {
    if (pre.querySelector('.code-banner')) continue;
    const code = pre.querySelector('code');
    if (!code) continue;
    const banner = el('div', 'code-banner');
    banner.dataset.uiAction = 'true';
    banner.append(el('span', undefined, '代码'));
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

function progressNode(existing, message, streaming) {
  const key = `progress:${message.id}`;
  let node = existing.get(key);
  if (!node) {
    node = el('div', 'message assistant narration');
    node.dataset.key = key;
    node.dataset.messageId = message.id;
    node.append(el('div', 'markdown narration-markdown'));
  }
  node.classList.toggle('streaming', !!streaming);
  const body = node.querySelector('.markdown');
  if (renderedSources.get(body) !== message.text) {
    renderedSources.set(body, message.text);
    renderMarkdown(body, message.text, { onLink: openMessageLink });
    addCodeActions(body);
  }
  return node;
}

function thoughtNode(existing, thought) {
  const key = `thought:${thought.id}`;
  let node = existing.get(key);
  if (!node) {
    node = el('details', 'thought-row');
    node.dataset.key = key;
    const summary = el('summary', 'thought-summary');
    summary.append(
      iconNode('chevronRight', { size: 12, className: 'thought-chevron' }),
      iconNode('sparkle', { size: 14 }),
      el('span', 'thought-label', '思考'),
      el('span', 'thought-preview'),
    );
    node.append(summary, el('div', 'thought-body'));
    node.addEventListener('toggle', () => {
      if (node.open) openThoughts.add(thought.id); else openThoughts.delete(thought.id);
    });
  }
  const text = thought.body ?? thought.text ?? '';
  node.querySelector('.thought-label').textContent = thought.interrupted ? '思考中断' : '思考';
  const preview = node.querySelector('.thought-preview');
  preview.textContent = text.replace(/\s+/g, ' ').slice(0, 160);
  node.querySelector('.thought-body').textContent = text;
  node.open = openThoughts.has(thought.id);
  return node;
}

function liveThoughtNode(existing, text) {
  let node = existing.get('thought:live');
  if (!node) {
    node = el('div', 'thought-row thought-live');
    node.dataset.key = 'thought:live';
    const head = el('div', 'thought-summary');
    head.append(iconNode('chevronRight', { size: 12, className: 'thought-chevron' }), iconNode('sparkle', { size: 14 }), el('span', 'thought-label', '思考中'));
    node.append(head, el('div', 'thought-body'));
  }
  node.querySelector('.thought-body').textContent = text;
  return node;
}

function receiptNode(existing, receipt, hiddenThoughts) {
  const key = `receipt:${receipt.members[0].id}`;
  let node = existing.get(key);
  if (!node) {
    node = el('details', 'thought-row');
    node.dataset.key = key;
    node.append(el('summary', 'thought-summary'), el('div', 'thought-body'));
  }
  node.querySelector('summary').textContent = `▸ ${receipt.label}`;
  const body = node.querySelector('.thought-body');
  body.replaceChildren();
  for (const thought of hiddenThoughts ?? []) {
    const text = thought.body ?? thought.text ?? '';
    body.append(el('div', `◆ ${thought.interrupted ? '思考中断' : '思考'}`), el('pre', undefined, text));
  }
  for (const member of receipt.members ?? []) body.append(el('div', `${member.name} ${member.target ?? ''}`));
  return node;
}

function toolKind(name) {
  const value = String(name ?? '').toLowerCase();
  if (/bash|shell|command|run_command|exec|terminal/.test(value)) return 'command';
  if (/read|cat|view/.test(value)) return 'read';
  if (/write|edit|patch/.test(value)) return 'edit';
  if (/web_(search|fetch)|fetch_web|search_web/.test(value)) return 'web';
  if (/grep|search|find|glob|list/.test(value)) return 'search';
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
    summary.append(lead, el('span', 'tool-label'), el('span', 'tool-target'), el('span', 'tool-duration'));
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
  const target = String(toolTarget(tool) ?? '');
  row.querySelector('.tool-target').textContent = waiting ? '等待你的批准'
    : target || (status === 'failed' ? failureReason(tool.preview ?? '') ?? '' : '');
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
  if (kind === 'command') {
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
    const pre = el('pre', 'tool-read-pre');
    const content = output.split('\n');
    for (const [index, line] of content.entries()) {
      const node = el('span', 'tool-read-line');
      node.append(el('span', 'tool-read-number', String(index + 1)), el('span', 'tool-read-text', line));
      pre.append(node);
    }
    card.append(head, pre);
  } else if (kind === 'edit') {
    card = patch ? renderConfirmedDiff(tool, patch) : el('div', 'tool-edit-card');
    if (!patch) card.append(el('pre', 'tool-output', output || '（无输出）'));
  } else if (kind === 'search') {
    card = el('div', 'tool-search-card');
    const head = el('div', 'tool-search-head');
    head.append(el('span', undefined, '搜索结果'), toolCopyButton(output));
    card.append(head, el('pre', 'tool-search-output', output || '（无结果）'));
  } else if (kind === 'web') {
    card = el('div', 'tool-web-card');
    const head = el('div', 'tool-web-head');
    head.append(el('span', undefined, typeof args?.url === 'string' ? args.url : typeof args?.query === 'string' ? args.query : '网页结果'));
    card.append(head, el('pre', 'tool-web-output', output || '（无内容）'));
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

function renderConfirmedDiff(tool, patch) {
  const node = el('div', 'confirmed-diff');
  node.dataset.patch = patch;
  const [added, removed] = diffCounts(patch);
  const head = el('div', 'confirmed-diff-head');
  head.append(el('span', 'confirmed-diff-title', '已确认改动'));
  head.append(el('span', 'confirmed-diff-stat add', `+${added}`), el('span', 'confirmed-diff-stat del', `−${removed}`));
  const copy = el('button', 'message-copy');
  copy.type = 'button';
  copy.setAttribute('aria-label', '复制完整 diff');
  copy.dataset.tooltip = '复制完整 diff';
  copy.append(iconNode('copy', { size: 14 }));
  copy.addEventListener('click', () => copyText(api, patch));
  head.append(copy);
  const body = el('pre', 'confirmed-diff-body');
  body.textContent = patch;
  node.append(head, body);
  return node;
}

function roundNode(existing, round) {
  const key = `round:${round.modelStep ?? 'legacy'}:${round.tools[0].id}`;
  let host = existing.get(key);
  if (!host) {
    host = el('div', 'execution-round');
    host.dataset.key = key;
    host.append(el('div', 'round-head'));
  }
  host.dataset.status = round.status;
  const waiting = round.tools.some(tool => current?.approvals.some(item => item.call_id === tool.id));
  const head = host.querySelector('.round-head');
  head.replaceChildren(
    el('span', 'round-glyph', waiting ? '◆' : roundGlyph(round)),
    el('span', 'round-label', waiting ? '等待审批' : roundHeadline(round)),
  );
  const kept = [];
  for (const tool of round.tools) {
    const node = toolNode(existing, tool);
    kept.push(node);
    if (node.parentNode !== host) host.append(node);
  }
  for (const child of [...host.children]) if (child !== head && !kept.includes(child)) child.remove();
  return host;
}

function processHost(existing, bucket) {
  const hostKey = `process:turn:${bucket.anchorId ?? '(tail)'}`;
  const children = [];
  const blocks = turnBlocks(bucket.thoughts, bucket.rounds);
  const items = [
    ...blocks,
    ...bucket.narration.map(message => ({ kind: 'narration', message, seq: message.seq ?? 0 })),
  ].sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0));
  for (const item of items) {
    if (item.kind === 'thought') children.push(thoughtNode(existing, item.thought));
    else if (item.kind === 'receipt') children.push(receiptNode(existing, item.receipt, item.thoughts));
    else if (item.kind === 'round') children.push(roundNode(existing, item.round));
    else if (item.kind === 'narration') children.push(progressNode(existing, item.message, current?.streamingMessageId === item.message.id));
  }
  if (bucket.live) children.push(bucket.live);
  if (!children.length) return null;
  let host = existing.get(hostKey);
  if (!host) {
    host = el('details', 'process-group');
    host.dataset.key = hostKey;
    const summary = el('summary', 'process-heading');
    summary.append(iconNode('chevronRight', { size: 14, className: 'chevron' }), el('span', 'process-label'), el('span', 'process-count'));
    host.append(summary, el('div', 'process-body'));
    host.addEventListener('toggle', () => {
      if (host.open) openProcess.add(hostKey); else openProcess.delete(hostKey);
    });
  }
  let toolCount = 0;
  for (const child of children) if (child.matches?.('.execution-round')) toolCount += child.querySelectorAll('.tool-row').length;
  const finished = current?.lastTerminal && !['running', 'waiting_user'].includes(current.status);
  host.querySelector('.process-label').textContent = finished ? '已完成' : '处理中';
  host.querySelector('.process-count').textContent = toolCount ? `${toolCount} 项` : '';
  const latestUser = [...current.messages].reverse().find(message => messageKind(message) === 'user')?.id;
  const keepLatest = settings.collapseAfter === 'next' && (bucket.anchorId ?? latestUser) === latestUser;
  const compact = settings.workSteps === 'compact';
  host.open = openProcess.has(hostKey) || (!compact && (!finished || keepLatest));
  const body = host.querySelector('.process-body');
  for (const child of [...body.children]) if (!children.includes(child)) child.remove();
  for (const child of children) if (child.parentNode !== body) body.append(child);
  return host;
}

function planCard() {
  const steps = current?.plan?.steps ?? [];
  if (!steps.length) return null;
  const done = steps.filter(step => step.status === 'done').length;
  const card = el('button', 'plan-card-row');
  card.type = 'button';
  card.dataset.key = 'plan-card';
  const icon = el('span', 'plan-card-icon');
  icon.append(iconNode('list', { size: 20 }));
  const copy = el('span', 'plan-card-copy');
  copy.append(el('span', 'plan-card-title', '执行计划'), el('span', 'plan-card-desc', `${done} / ${steps.length} 步骤已完成`));
  card.append(icon, copy, el('span', 'plan-card-open', '打开'));
  card.addEventListener('click', () => openRightbarTab('plan'));
  return card;
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

function changedFilesCard() {
  const files = current?.diff?.files ?? [];
  if (!files.length) return null;
  const card = el('div', 'changed-files-card');
  card.dataset.key = 'changed-files';
  const head = el('button', 'changed-files-head');
  head.type = 'button';
  const icon = el('span', 'delivered-icon');
  icon.append(iconNode('code', { size: 20 }));
  const copy = el('span', 'delivered-copy');
  const added = files.reduce((sum, file) => sum + (file.added ?? 0), 0);
  const removed = files.reduce((sum, file) => sum + (file.removed ?? 0), 0);
  copy.append(
    el('span', 'delivered-name', files.length === 1 ? `已修改 ${files[0].path.split('/').at(-1)}` : `已修改 ${files.length} 个文件`),
    el('span', 'delivered-meta', `${files.length} 个文件 · +${added} −${removed}`),
  );
  head.append(icon, copy);
  head.addEventListener('click', () => openRightbarTab('changes'));
  card.append(head);
  for (const file of files.slice(0, 4)) {
    const row = el('button', 'changed-file-row');
    row.type = 'button';
    row.append(el('span', 'changed-file-path', file.path), el('span', 'changed-file-add', `+${file.added ?? 0}`), el('span', 'changed-file-del', `−${file.removed ?? 0}`));
    row.addEventListener('click', () => {
      const tab = openRightbarTab('changes');
      if (tab) { tab.selected = file.path; persistRightbar(); renderRightbar(); }
    });
    card.append(row);
  }
  if (files.length > 4) card.append(el('div', 'changed-files-more', `其余 ${files.length - 4} 个文件`));
  return card;
}

function renderConversation() {
  const list = $('conversation');
  const scroller = $('conversation-scroll');
  const nearBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 120;
  if (!current) {
    list.replaceChildren();
    document.body.classList.toggle('home', true);
    document.body.classList.toggle('blank-session', true);
    return;
  }
  const home = isHome();
  document.body.classList.toggle('home', home);
  document.body.classList.toggle('blank-session', home);
  const existing = new Map([...list.querySelectorAll('[data-key]')].map(node => [node.dataset.key, node]));
  const nodes = [];
  const userTurns = [];
  for (const message of current.messages) if (messageKind(message) === 'user') userTurns.push({ id: message.id, seq: message.seq ?? 0 });
  const seqOf = new Map(current.messages.map(message => [message.id, message.seq ?? 0]));
  const turnOf = (anchor, seq) => {
    let at = anchor != null ? seqOf.get(anchor) : undefined;
    if (at === undefined) at = seq ?? 0;
    let index = -1;
    for (let i = 0; i < userTurns.length; i += 1) { if (userTurns[i].seq <= at) index = i; else break; }
    return index;
  };
  const buckets = new Map();
  const bucketFor = index => {
    if (!buckets.has(index)) buckets.set(index, { index, anchorId: index >= 0 ? userTurns[index].id : null, thoughts: [], rounds: [], narration: [], live: null });
    return buckets.get(index);
  };
  for (const thought of foldedThoughts(current.thoughts ?? [])) bucketFor(turnOf(thought.anchor, thought.seq)).thoughts.push(thought);
  for (const round of groupExecutionRounds(current.tools)) {
    const first = round.tools[0];
    bucketFor(turnOf(first.anchor, first.seq)).rounds.push(round);
  }
  const messageTurn = new Map();
  let turnIndex = -1;
  for (const message of current.messages) {
    if (messageKind(message) === 'user') turnIndex += 1;
    messageTurn.set(message.id, turnIndex);
  }
  const liveText = (current.reasoning ?? '').trim();
  const currentTurn = userTurns.length ? userTurns.length - 1 : -1;
  if (liveText !== '') bucketFor(currentTurn);
  const demoted = new Set();
  for (const message of current.messages) {
    if (messageKind(message) !== 'assistant' || message.final === true || String(message.text ?? '').trim() === '') continue;
    const index = messageTurn.get(message.id) ?? -1;
    const bucket = bucketFor(index);
    if (bucket.thoughts.length > 0 || bucket.rounds.length > 0 || (liveText !== '' && index === currentTurn)) {
      bucket.narration.push(message);
      demoted.add(message.id);
    }
  }
  for (const message of current.messages) {
    if (demoted.has(message.id)) continue;
    if (messageKind(message) === 'assistant' && String(message.text ?? '').trim() === '') continue;
    const node = messageRow(existing, message);
    node.classList.toggle('flow-response', messageKind(message) === 'assistant');
    node.classList.toggle('flow-turn', messageKind(message) === 'user');
    nodes.push(node);
  }
  const lastIndex = buckets.size ? Math.max(...buckets.keys()) : -1;
  const placed = [];
  for (const [index, bucket] of [...buckets.entries()].sort((a, b) => a[0] - b[0])) {
    if (index === lastIndex && liveText !== '') bucket.live = liveThoughtNode(existing, current.reasoning);
    const host = processHost(existing, bucket);
    if (host) placed.push({ host, anchorId: bucket.anchorId });
  }
  for (const { host, anchorId } of placed) {
    const anchorNode = anchorId ? nodes.find(node => node.dataset.messageId === anchorId) : null;
    const position = anchorNode ? nodes.indexOf(anchorNode) + 1 : nodes.length;
    nodes.splice(position, 0, host);
  }
  const running = current.status === 'running';
  if (running) {
    runStartedAt ??= Date.now();
    let row = existing.get('running');
    if (!row) {
      row = el('div', 'running-row');
      row.dataset.key = 'running';
      row.append(el('span', 'running-dot'), el('span', 'running-text'));
    }
    row.querySelector('.running-text').textContent = runningLabel();
    nodes.push(row);
  }
  if (running && !runClockTimer) {
    runClockTimer = setInterval(() => {
      const text = $('conversation').querySelector('.running-row .running-text');
      if (text) text.textContent = runningLabel();
    }, 1000);
  }
  if (!running && runClockTimer) { clearInterval(runClockTimer); runClockTimer = null; runStartedAt = null; }
  for (const error of [...(current.turnErrors ?? [])].sort((a, b) => a.seq - b.seq)) {
    const anchorIndex = error.anchor ? nodes.findIndex(node => node.dataset.messageId === error.anchor) : -1;
    const hostIndex = error.anchor ? nodes.findIndex(node => node.dataset.key === `process:turn:${error.anchor}`) : -1;
    const position = hostIndex >= 0 ? hostIndex + 1 : anchorIndex >= 0 ? anchorIndex + 1 : nodes.length;
    nodes.splice(position, 0, turnErrorNode(error));
  }
  const plan = planCard(); if (plan) nodes.push(plan);
  const delivered = deliveredCard(); if (delivered) nodes.push(delivered);
  const changed = changedFilesCard(); if (changed) nodes.push(changed);
  const usage = usageNode(); if (usage) nodes.push(usage);
  const keep = new Set(nodes);
  for (const child of [...list.children]) if (!keep.has(child)) child.remove();
  for (let i = 0; i < nodes.length; i += 1) if (list.children[i] !== nodes[i]) list.insertBefore(nodes[i], list.children[i] ?? null);
  if (nearBottom) scroller.scrollTop = scroller.scrollHeight;
  if (!$('find-bar').hidden) updateFind();
}

function turnErrorNode(error) {
  const row = el('div', 'turn-error-row');
  row.dataset.key = `turn-error:${error.id}`;
  row.append(el('span', 'turn-error-dot'), el('span', 'turn-error-title', '本轮失败'), el('span', 'turn-error-text', error.text));
  return row;
}

function runningLabel() {
  const elapsed = runStartedAt ? Math.floor((Date.now() - runStartedAt) / 1000) : 0;
  return elapsed >= 1 ? `深度求索中，用时 ${elapsed}s ···` : '深度求索中 ···';
}

function usageNode() {
  if (!current?.usage) return null;
  const usage = current.usage;
  if (!usage.input_tokens && !usage.output_tokens) return null;
  const parts = [`输入 ${usage.input_tokens ?? 0}`, `输出 ${usage.output_tokens ?? 0}`];
  if (usage.cached_input_tokens) parts.push(`缓存 ${usage.cached_input_tokens}`);
  if (settings.performance === 'detailed' && usage.reasoning_tokens) parts.push(`推理 ${usage.reasoning_tokens}`);
  const row = el('div', 'turn-usage', parts.join(' · '));
  row.dataset.key = 'usage';
  return row;
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
  timeline.append(range);
  const fractionAt = clientX => {
    const track = timeline.querySelector('.trajectory-lane-track');
    if (!track) return 0;
    const rect = track.getBoundingClientRect();
    return Math.max(0, Math.min(1, (clientX - rect.left) / Math.max(1, rect.width)));
  };
  const updateRange = (startFraction, endFraction) => {
    const total = Number(timeline.dataset.total) || 1;
    const low = Math.min(startFraction, endFraction);
    const high = Math.max(startFraction, endFraction);
    trajectoryRange = high - low < 0.002 ? null : { start: low * total, end: high * total };
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
    trajectoryRange = null;
    range.hidden = true;
    applyTrajectoryRange();
  });
  const body = el('div', 'trajectory-body');
  const ledger = el('div', 'trajectory-ledger');
  ledger.tabIndex = 0;
  ledger.addEventListener('scroll', () => {
    trajectoryPinned = ledger.scrollHeight - ledger.scrollTop - ledger.clientHeight < 80;
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
  host.dataset.total = String(timeline.total);
  const lanes = [['input', '输入'], ['model', '模型'], ['tool', '工具']];
  for (const [laneId, label] of lanes) {
    const row = el('div', 'trajectory-lane');
    row.append(el('span', 'trajectory-lane-label', label));
    const track = el('div', 'trajectory-lane-track');
    for (const item of timeline.items.filter(entry => entry.lane === laneId)) {
      const bar = el('span', 'trajectory-bar');
      bar.dataset.kind = item.kind;
      bar.dataset.start = String(item.start);
      bar.dataset.end = String(item.end);
      bar.style.left = `${(item.start / timeline.total) * 100}%`;
      bar.style.width = `max(2px, calc(${((item.end - item.start) / timeline.total) * 100}% - 2px))`;
      const record = records.find(entry => entry.id === item.id);
      bar.title = record ? `${record.label} · ${formatDuration(record.startedMs) ?? '计时不可用'}` : '';
      track.append(bar);
    }
    row.append(track);
    host.append(row);
  }
}

function applyTrajectoryRange() {
  const root = $('trajectory-root');
  if (!root) return;
  const total = Number(root.querySelector('.trajectory-timeline')?.dataset.total) || 1;
  const overlay = root.querySelector('.trajectory-range');
  if (overlay) {
    overlay.hidden = trajectoryRange === null;
    if (trajectoryRange) {
      const timeline = root.querySelector('.trajectory-timeline');
      const track = timeline?.querySelector('.trajectory-lane-track');
      if (timeline && track) {
        const trackRect = track.getBoundingClientRect();
        const hostRect = timeline.getBoundingClientRect();
        const low = Math.max(0, Math.min(trajectoryRange.start, trajectoryRange.end) / total);
        const high = Math.min(1, Math.max(trajectoryRange.start, trajectoryRange.end) / total);
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
    card.append(el('p', undefined, request.summary ?? ''));
    if (request.command) card.append(el('div', 'interaction-command', request.command));
    if (request.risks?.length) {
      const risks = el('ul', 'interaction-risks');
      for (const risk of request.risks) risks.append(el('li', undefined, risk));
      card.append(risks);
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
    if (request.question) card.append(el('p', undefined, request.question));
    const questions = request.questions?.length ? request.questions : [{ question: request.question, options: request.options, kind: 'single', allow_other: true }];
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
  items.push({ label: '打开本地文件夹…', icon: 'folderPlus', onSelect: () => action(async () => {
    const folder = await api.chooseFolder();
    if (folder) selectWorkspace(folder);
  }) });
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
    ['commands', 'goal', '目标', false, 'compass', '未接入', 'Runtime 没有长期目标编辑接口'],
    ['commands', 'plan', '计划', false, 'list', '未接入', '桌面尚未接入计划模式切换'],
    ['commands', 'feedback', '反馈', true, 'feedback', null, null],
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
  $('message').value = '';
  composerError('');
  controls();
  if (id === 'feedback') { openFeedbackDialog(); return; }
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
  activeView = 'chat';
  liveTrajectory = [];
  liveTurnStartAt = null;
  liveTurnClosed = false;
  trajectorySelectedId = null;
  trajectoryNodeLimit = 50;
  trajectoryRange = null;
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
  activeView = savedView(current.session.id);
  liveTrajectory = [];
  liveTurnStartAt = null;
  liveTurnClosed = false;
  trajectorySelectedId = null;
  trajectoryNodeLimit = 50;
  trajectoryRange = null;
  $('conversation').replaceChildren();
  loadRightbar(current.session.id);
  render();
  if (draftText) {
    $('message').value = draftText;
    resizeComposer();
    drafts.set(current.session.id, draftText);
    drafts.delete('new-session');
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
  const folder = await api.chooseRecentWorkspace(path);
  if (!folder) return;
  selection = folder;
  current = null;
  history = [];
  $('message').value = drafts.get('new-session') ?? '';
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
  if (task.primary_workspace) foldedGroups.delete(task.primary_workspace);
  currentSource = result.source_id ?? task.source_id;
  current = attachExtras(projectSnapshot(result.session), result.session);
  activeView = savedView(current.session.id);
  liveTrajectory = [];
  liveTurnStartAt = null;
  liveTurnClosed = false;
  trajectorySelectedId = null;
  trajectoryNodeLimit = 50;
  trajectoryRange = null;
  $('message').value = drafts.get(current.session.id) ?? '';
  $('conversation').replaceChildren();
  loadRightbar(current.session.id);
  render();
  await loadHistory();
  setTimeout(refreshContextUsage, 80);
  $('conversation-scroll').scrollTop = $('conversation-scroll').scrollHeight;
  closeNarrowSidebar();
}

function attachExtras(state, snapshot) {
  return {
    ...state,
    children: snapshot.children ?? state.children ?? [],
    backgroundTasks: snapshot.active_background_tasks ?? state.backgroundTasks ?? [],
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
  const prior = current;
  const diffQuery = current.diffQuery;
  const diffError = current.diffError;
  current = attachExtras(retainInterruptedThoughts(projectSnapshot(snapshot, history), prior), snapshot);
  current.diffQuery = diffQuery;
  if (!snapshot.diff) current.diffError = diffError;
  render();
}

async function loadHistory() {
  if (!current) return;
  await deliver({ type: 'query_session_history', session_id: current.session.id, query_id: crypto.randomUUID() });
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
    const problem = item.error?.message ? attachmentErrorPresentation(item.error.message) : null;
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

async function openMessageLink(url) {
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
  const turns = (current?.messages ?? []).filter(message => messageKind(message) === 'user').length;
  return `第 ${Math.max(1, turns)} 轮改动`;
}

function newTabRecord(kind, extra = {}) {
  const id = `tab${rightbarSeq++}`;
  const title = {
    start: '开始', files: '文件', terminal: '终端', browser: '浏览器', changes: '变更',
    preview: extra.title ?? '预览', deliverables: '交付', plan: '计划', subagent: '子智能体',
  }[kind] ?? kind;
  return { id, kind, title, ...extra };
}

function ensureRightbarSession() {
  const key = current?.session.id ?? 'new-session';
  if (key !== rightbarSessionKey) loadRightbar(key);
}
function activePane() { return rightbarPanes.find(pane => pane.id === rightbarActivePane) ?? rightbarPanes[0]; }

function openRightbarTab(kind, extra = {}) {
  ensureRightbarSession();
  if (kind === 'changes' && !hasWorkspace()) { toast('当前会话没有工作区', { tone: 'warning' }); return null; }
  const pane = activePane();
  const active = pane.activeId ? rightbarTabs[pane.activeId] : null;
  if (active?.kind === 'start') {
    active.kind = kind;
    active.title = extra.title ?? ({ files: '文件', terminal: '终端', browser: '浏览器', changes: changesTabTitle(), preview: '预览' }[kind] ?? active.title);
    Object.assign(active, extra);
    rightbarOpen = true;
    rightbarExpanded = false;
    persistRightbar();
    renderRightbar();
    if (kind === 'changes') requestDiff();
    return active;
  }
  const reusable = kind === 'preview'
    ? Object.values(rightbarTabs).find(tab => tab.kind === 'preview' && tab.path === extra.path && !rightbarFloats.some(float => float.tabId === tab.id))
    : ['files', 'changes', 'plan', 'deliverables', 'subagent'].includes(kind)
      ? Object.values(rightbarTabs).find(tab => tab.kind === kind && !rightbarFloats.some(float => float.tabId === tab.id)) : null;
  const tab = reusable ?? newTabRecord(kind, extra);
  if (kind === 'changes' && !extra.title) tab.title = changesTabTitle();
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
  if (kind === 'changes') requestDiff();
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
function closeTab(pane, tabId) {
  const index = pane.tabs.indexOf(tabId);
  if (index < 0) return;
  const closing = rightbarTabs[tabId];
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
  if (innerWidth < 768) { closeRightbar(); return; }
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
      button.draggable = true;
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
      const movedId = event.dataTransfer.getData('text/plain');
      const source = rightbarPanes.find(item => item.tabs.includes(movedId));
      if (!source || !rightbarTabs[movedId]) return;
      source.tabs = source.tabs.filter(id => id !== movedId);
      if (source.activeId === movedId) source.activeId = source.tabs[0] ?? null;
      const target = event.target.closest('.pane-tab');
      const index = target ? pane.tabs.indexOf(target.dataset.tabId) : pane.tabs.length;
      pane.tabs.splice(index < 0 ? pane.tabs.length : index, 0, movedId);
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

function removeFloat(float) {
  const closing = rightbarTabs[float.tabId];
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
}

function renderTabView(tab, pane) {
  if (tab.kind === 'start') return startView(tab);
  if (tab.kind === 'files') return filesView(tab);
  if (tab.kind === 'terminal') return terminalView();
  if (tab.kind === 'browser') return browserView(tab, pane);
  if (tab.kind === 'changes') return changesView(tab);
  if (tab.kind === 'preview') return previewView(tab);
  if (tab.kind === 'deliverables') return deliverablesView();
  if (tab.kind === 'plan') return planView();
  if (tab.kind === 'subagent') return subagentView();
  return startView(tab);
}

function startView(tab) {
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
  const launch = kind => {
    tab.kind = kind;
    tab.title = kind === 'files' ? '文件' : kind === 'terminal' ? '终端' : '浏览器';
    persistRightbar();
    renderRightbar();
  };
  cards.append(card('files', 'folder', 'folder', '工作区文件', '浏览会话工作区的文件', '⌘ P', () => launch('files'), '桌面桥接没有目录读取接口'));
  cards.append(card('terminal', 'terminal', 'terminal', '新建终端', '在会话工作区运行命令', '⌃ `', () => launch('terminal'), '桌面桥接没有交互式 PTY'));
  cards.append(card('browser', 'globe', 'globe', '浏览器', '浏览网页', '⌘ T', () => launch('browser')));
  guide.append(mark, cards);
  return guide;
}

function filesView() {
  const view = el('div', 'files-view');
  const path = current?.session.repository ?? '';
  const toolbar = el('div', 'view-toolbar');
  const pathNode = el('div', 'toolbar-path');
  if (path) {
    const last = path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
    pathNode.append(document.createTextNode(path.slice(0, path.length - last.length)), el('strong', undefined, last));
  } else pathNode.textContent = '未选择工作区';
  pathNode.title = path;
  const refresh = iconButton('refresh', '重新读取', { size: 15, tooltip: '重新读取' });
  refresh.disabled = true;
  const open = el('div', 'open-in-app-pane');
  toolbar.append(pathNode, refresh, open);
  renderOpenInAppInto(open, path, 'directory');
  const body = el('div', 'files-body');
  if (!path) {
    const state = el('div', 'files-empty');
    state.append(el('div', undefined, '当前会话没有工作区。先选择一个本地文件夹。'));
    const choose = el('button', 'button secondary small', '选择工作区');
    choose.type = 'button';
    choose.addEventListener('click', () => $('workspace-picker').click());
    state.append(choose);
    body.append(state);
  } else {
    const note = el('div', 'files-note');
    note.append(el('span', undefined, '桌面桥接没有目录读取接口，无法列出真实文件树。'));
    body.append(note);
  }
  view.append(toolbar, body);
  return view;
}

function terminalView() {
  const view = el('div', 'terminal-view');
  const screen = el('div', 'terminal-screen');
  const box = el('div', 'terminal-offline');
  const mark = el('span', 'terminal-offline-icon');
  mark.append(iconNode('terminal', { size: 36, stroke: 1.1 }));
  box.append(mark, el('h3', undefined, '交互式终端尚未接入'));
  box.append(el('p', undefined, '桌面桥接没有可安全复用的 PTY。Agent 命令仍在对话中执行并展示完整输出。'));
  screen.append(box);
  const status = el('div', 'terminal-status');
  status.append(el('span', 'terminal-status-detail', '没有可用 Shell'));
  view.append(screen, status);
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
  if (!tab.browserId && !tab.creating) void ensureBrowserTab(tab).then(() => { if (rightbarOpen) renderRightbar(); });
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
    browserCommand(data ? 'navigate' : 'new', { url }, tab);
  };
  address.addEventListener('input', () => { tab.addressDraft = address.value; tab.addressDirty = true; });
  address.addEventListener('keydown', event => {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    navigateAddress();
  });
  address.addEventListener('blur', () => {
    if (!tab.addressDirty) tab.addressDraft = undefined;
    tab.addressDirty = false;
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
  toolbar.append(back, forward, reload, addressWrap, external);
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
  const active = candidates[0] ?? null;
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

function reviewLineNode(line) {
  const node = el('div', `review-line ${line.kind}`);
  node.append(
    el('span', 'ln', line.old === null ? '' : String(line.old)),
    el('span', 'ln', line.next === null ? '' : String(line.next)),
    el('span', 'marker', line.kind === 'added' ? '+' : line.kind === 'removed' ? '−' : ''),
    el('span', 'text', line.text),
  );
  return node;
}

function reviewSplit(lines) {
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
    node.append(
      el('span', 'ln', number === null ? '' : String(number)),
      el('span', 'marker', line.kind === 'added' ? '+' : line.kind === 'removed' ? '−' : ''),
      el('span', 'text', line.text),
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
  if (hasWorkspace() && !current.diff && !current.diffError && current.diffQuery?.status !== 'pending') setTimeout(requestDiff, 0);
  const view = el('div');
  view.style.display = 'flex'; view.style.flexDirection = 'column'; view.style.flex = '1'; view.style.minHeight = '0';
  const files = current?.diff?.files ?? [];
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
  const split = iconButton('columns', '切换左右对比', { size: 15, tooltip: '切换左右对比' });
  split.setAttribute('aria-pressed', String(mode === 'split'));
  split.addEventListener('click', () => { tab.mode = mode === 'split' ? 'unified' : 'split'; persistRightbar(); renderRightbar(); });
  const wrap = iconButton('wrap', '切换换行', { size: 15, tooltip: '切换换行' });
  wrap.setAttribute('aria-pressed', String(!!tab.wrap));
  wrap.addEventListener('click', () => { tab.wrap = !tab.wrap; renderRightbar(); });
  const open = iconButton('external', '打开完整文件', { size: 15, tooltip: '打开完整文件' });
  open.disabled = !selected;
  open.addEventListener('click', () => { if (selected) openPreviewForPath(selected.path); });
  const refresh = iconButton('refresh', '重新读取', { size: 15, tooltip: '重新读取' });
  refresh.disabled = current?.diffQuery?.status === 'pending';
  refresh.addEventListener('click', requestDiff);
  const openApp = el('div', 'open-in-app-pane');
  tools.append(split, wrap, open, refresh, openApp);
  if (selected?.path) renderOpenInAppInto(openApp, selected.path, 'file');
  head.append(tools);
  view.append(head);
  const body = el('div', 'review-body');
  if (tab.wrap) body.classList.add('wrap');
  if (current?.diffQuery?.status === 'pending' && !current.diff) body.append(el('div', 'review-status', '正在读取变更…'));
  else if (current?.diffError) {
    const status = el('div', 'review-status error', current.diffError);
    const retry = el('button', 'button secondary small', '重新读取');
    retry.type = 'button';
    retry.addEventListener('click', requestDiff);
    status.append(retry);
    body.append(status);
  } else if (!selected) body.append(el('div', 'review-status', hasWorkspace() ? '工作区没有变更' : '当前会话没有工作区'));
  else if (!selected.patch) body.append(el('div', 'review-status', '当前快照未包含补丁正文。完整内容请查看对话中的已确认改动。'));
  else {
    const lines = parsePatch(selected.patch);
    const oneSided = !lines.some(line => line.kind === 'removed') || !lines.some(line => line.kind === 'added');
    if (mode === 'split' && !oneSided) {
      const splitView = reviewSplit(lines);
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
      for (const line of lines.slice(0, 5000)) diff.append(reviewLineNode(line));
      body.append(diff);
      if (lines.length > 5000) body.append(el('div', 'review-status', `变更较大，仅显示前 5000 行（共 ${lines.length} 行）。`));
    }
  }
  view.append(body);
  return view;
}

function previewRenderers(path) {
  const extension = String(path ?? '').split('.').pop()?.toLowerCase() ?? '';
  if (['md', 'markdown'].includes(extension)) return [{ id: 'markdown', label: 'Markdown' }, { id: 'text', label: '纯文本' }];
  if (['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'ico', 'svg'].includes(extension)) return [{ id: 'image', label: '图片' }];
  if (extension === 'pdf') return [{ id: 'pdf', label: 'PDF' }];
  if (['xlsx', 'xls', 'csv', 'tsv'].includes(extension)) return [{ id: 'excel', label: '表格' }, { id: 'text', label: '纯文本' }];
  if (['doc', 'docx', 'ppt', 'pptx'].includes(extension)) return [{ id: 'office', label: 'Office' }];
  if (['html', 'htm'].includes(extension)) return [{ id: 'html', label: 'HTML' }, { id: 'text', label: '纯文本' }];
  return [{ id: 'code', label: '代码' }, { id: 'text', label: '纯文本' }];
}

function previewCodeBody(content, path) {
  const wrap = el('div', 'preview-code');
  const lines = String(content).split('\n');
  const banner = el('div', 'preview-code-banner');
  banner.append(el('span', undefined, String(path ?? '').split('.').pop()?.toLowerCase() || 'text'));
  const copy = el('button', 'preview-code-copy');
  copy.type = 'button';
  copy.setAttribute('aria-label', '复制');
  copy.append(iconNode('copy', { size: 16 }));
  copy.addEventListener('click', () => copyText(api, content));
  banner.append(copy);
  const pre = el('pre', 'preview-code-pre');
  const code = el('code');
  for (const [index, line] of lines.entries()) {
    const row = el('span', 'preview-code-line');
    row.append(el('span', 'preview-line-number', String(index + 1)), el('span', 'preview-line-text', line));
    code.append(row);
  }
  pre.append(code);
  wrap.append(banner, pre);
  return wrap;
}

function openPreviewForPath(path, title = null) {
  if (!path) return null;
  const renderers = previewRenderers(path);
  return openRightbarTab('preview', { path, title: title ?? String(path).split(/[\\/]/).pop() ?? '预览', renderer: renderers[0].id, renderers });
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
    items: renderers.map(item => ({ label: item.label, checked: item.id === selected.id, onSelect: () => { tab.renderer = item.id; persistRightbar(); renderRightbar(); } })),
  }));
  const wrap = iconButton('wrap', '自动换行', { size: 15, tooltip: tab.wrap ? '关闭自动换行' : '自动换行' });
  wrap.setAttribute('aria-pressed', String(!!tab.wrap));
  wrap.addEventListener('click', () => { tab.wrap = !tab.wrap; persistRightbar(); renderRightbar(); });
  const reload = iconButton('refresh', '重新读取文件', { size: 15, tooltip: '重新读取文件' });
  reload.addEventListener('click', () => toast('桌面桥接没有文件读取接口', { tone: 'info', holdMs: 2400 }));
  const open = el('div', 'open-in-app-pane');
  toolbar.append(pathNode, renderer, wrap, reload, open);
  renderOpenInAppInto(open, path, 'file');
  const body = el('div', `preview-body${tab.wrap ? ' wrap' : ''}`);
  if (tab.content) {
    if (selected.id === 'markdown') {
      const markdown = el('div', 'markdown');
      renderMarkdown(markdown, tab.content, { onLink: openMessageLink });
      addCodeActions(markdown);
      body.append(markdown);
    } else if (selected.id === 'code') body.append(previewCodeBody(tab.content, path));
    else body.append(el('pre', undefined, tab.content));
  } else {
    const empty = el('div', 'right-empty');
    const mark = el('span', 'empty-icon'); mark.append(iconNode('file', { size: 48, stroke: 1.1 }));
    empty.append(mark, el('h3', undefined, '文件预览尚未接入'), el('p', undefined, '桌面桥接没有文件读取接口，无法在应用内安全打开工作区文件。'));
    const openHost = el('div', 'open-in-app-unpreviewable');
    empty.append(openHost);
    renderOpenInAppInto(openHost, path, 'file');
    body.append(empty);
  }
  view.append(toolbar, body);
  return view;
}

function deliverablesView() {
  const report = current?.session?.completion_report ?? current?.completionReport;
  if (!report) {
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
  note.append(el('span', undefined, 'Runtime 只提供聚合统计，没有交付文件清单；不会伪造文件名或预览。'));
  view.append(card, note);
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
  view.append(el('h2', 'plan-title', '计划'));
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
    const head = el('div', 'subagent-head');
    head.append(iconNode('bot', { size: 16 }), el('span', 'subagent-name', child.nickname || child.title || child.id));
    const state = child.done ? (child.ok === false ? '失败' : '已完成') : child.phase || '运行中';
    const badge = el('span', 'subagent-state', state);
    badge.dataset.done = String(!!child.done);
    head.append(badge);
    card.append(head);
    const detail = child.phase || child.tool || child.preview || child.detail || child.scope || child.role;
    if (detail) card.append(el('div', 'subagent-detail', String(detail)));
    if (child.input_tokens || child.output_tokens) {
      card.append(el('div', 'subagent-usage', `输入 ${child.input_tokens ?? 0} · 输出 ${child.output_tokens ?? 0}`));
    }
    view.append(card);
  }
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

function renderGeneralSettings(content) {
  const group = el('div', 'settings-group');
  group.append(settingRow('权限', '选择新会话的默认权限模式', menuControl(settings.defaultPermission, PERMISSION_MODES.map(([value, label]) => ({ value, label })), value => {
    settings.defaultPermission = value; saveSettings(); toast('默认权限已更新', { tone: 'success', holdMs: 1600 });
  }, { label: '权限' })));
  group.append(settingRow('语言', '', menuControl('zh-CN', [{ value: 'zh-CN', label: '中文' }], () => {}, { label: '语言' })));
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
  group.append(settingRow('工作步骤展示', '选择希望看到多少工具调用细节', menuControl(settings.workSteps, [
    { value: 'compact', label: '简洁' }, { value: 'standard', label: '标准' },
    { value: 'detailed', label: '详细' }, { value: 'verbose', label: '完全展开' },
  ], value => { settings.workSteps = value; saveSettings(); renderConversation(); }, { label: '工作步骤展示' })));
  group.append(settingRow('工作步骤收起时机', '选择何时自动收起工作步骤', menuControl(settings.collapseAfter, [
    { value: 'answer', label: '回答结束后' }, { value: 'next', label: '下次有新消息时' },
  ], value => { settings.collapseAfter = value; saveSettings(); renderConversation(); }, { label: '工作步骤收起时机' })));
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
  card.append(el('h3', undefined, '添加模型提供商'), el('p', undefined, 'Provider、密钥与 Base URL 的写入接口尚未接入桌面。'), unconnected());
  const add = el('button', 'button secondary', '添加模型提供商');
  add.type = 'button'; add.disabled = true;
  card.append(add);
  content.append(card);
}

function renderPluginSettings(content) {
  content.append(el('h2', 'settings-title-large', '内置插件'));
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
    tags.append(el('span', 'preset-tag', agent.status ?? 'unknown'), unconnected('切换未接入'));
    card.append(tags);
    content.append(card);
  }
  for (const problem of agentsRead.data?.problems ?? []) content.append(el('div', 'settings-note', `${problem.source}: ${problem.error}`));
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
    const wrap = el('div', 'settings-actions'); wrap.append(action);
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
    const card = el('div', 'entry-card');
    card.append(el('h2', undefined, '调度接口尚未接入'), el('p', undefined, 'CodeLeveler Runtime 暴露了 schedule 工具与事件，但桌面桥接层没有列表/创建/取消接口。这里不会展示伪造的计划记录。'));
    page.append(card);
    const empty = el('div', 'entry-empty');
    const mark = el('span', 'empty-mark');
    mark.append(iconNode('automation', { size: 44, stroke: 1.1 }));
    empty.append(mark, document.createTextNode('暂无自动化任务'));
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
    const prior = current;
    const diffQuery = current?.diffQuery;
    const diffError = current?.diffError;
    const priorContext = current?.contextUsage ?? null;
    current = attachExtras(projectSnapshot(frame.data, history), frame.data);
    current.contextUsage = priorContext;
    current.diffQuery = diffQuery;
    if (!frame.data.diff) current.diffError = diffError;
    render();
    return;
  }
  if (frame.event !== 'runtime') return;
  const event = frame.data;
  recordLiveTrajectory(event);
  if (event.type === 'session_history_loaded') {
    history = event.entries ?? [];
    liveTrajectory = [];
    const restored = retainInterruptedThoughts(projectSnapshot(current.session, history), current);
    current = { ...current, messages: restored.messages, tools: restored.tools, thoughts: restored.thoughts, lastTerminal: restored.lastTerminal ?? current.lastTerminal, turnErrors: restored.turnErrors ?? [] };
    renderConversation();
    if (activeView === 'trajectory') renderTrajectory();
    return;
  }
  if (event.type === 'session_opened') {
    const priorContext = current?.contextUsage ?? null;
    current = attachExtras(projectSnapshot(event.session, history), event.session);
    current.contextUsage = priorContext;
    liveTrajectory = [];
    liveTurnStartAt = null;
    liveTurnClosed = false;
    render();
    return;
  }
  if (event.type === 'session_updated') {
    current = attachExtras(applySessionMetadata(current, event.session), event.session);
    renderHeader();
    renderComposerMeta();
    renderSidebar();
    return;
  }
  if (event.type === 'token_usage') {
    current = { ...current, usage: { ...(current.usage ?? {}), ...event } };
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
  if (event.type.startsWith('background_task_')) {
    current = applyBackgroundEvent(current, event);
    renderHeader();
    return;
  }
  if (event.type === 'notification') {
    if (event.level === 'warning' || event.level === 'error') toast(event.message, { tone: event.level, holdMs: 5000 });
    return;
  }
  if (event.type === 'agents_loaded') return;
  const pendingDiff = current.diffQuery?.status === 'pending' && event.query_id === current.diffQuery.id;
  current = applyEvent(current, event);
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
  if (['diff_updated', 'diff_failed'].includes(event.type) && event.query_id === current.diffQuery?.id
    && ['confirmed', 'failed'].includes(current.diffQuery.status)) {
    if (pendingDiff && rightbarOpen) {
      const tab = Object.values(rightbarTabs).find(item => item.kind === 'changes');
      if (tab && !tab.selected) tab.selected = current.diff?.files?.[0]?.path ?? null;
    }
    renderRightbar();
    renderConversation();
  } else if (event.type === 'plan_updated') {
    renderRightbar();
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
  if (event.type === 'background_task_started') jobs.push({ task_id: event.task_id, program: event.program, args: event.args, pid: event.pid, elapsed_ms: 0 });
  if (event.type === 'background_task_exited') return { ...state, backgroundTasks: jobs.filter(task => task.task_id !== event.task_id) };
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
  ensureHeaderActions();
  applyTheme();
  applyContentFont();
  api.appInfo?.().then(info => { $('brand-version').textContent = `v${info.version}`; }).catch(() => {});
  api.onBrowserState(data => {
    browserData = data;
    for (const tab of Object.values(rightbarTabs)) {
      if (tab.kind !== 'browser' || !tab.browserId) continue;
      const found = data.tabs.find(item => item.id === tab.browserId);
      if (!found) { tab.browserId = null; continue; }
      tab.title = found.url && found.url !== 'about:blank' ? browserChrome(found).title : '浏览器';
    }
    if (rightbarOpen) renderRightbar(); else scheduleBrowserSurface();
  });
  api.onOpenSettings?.(() => openSettings('general'));
  window.addEventListener('ui-overlay-change', () => scheduleBrowserSurface());

  $('new-session').addEventListener('click', newSession);
  $('leading-new').addEventListener('click', newSession);
  $('sidebar-toggle').addEventListener('click', toggleSidebar);
  $('leading-toggle').addEventListener('click', toggleSidebar);
  $('nav-plugins').addEventListener('click', () => openPage('plugins'));
  $('nav-automation').addEventListener('click', () => openPage('automation'));
  $('session-search').addEventListener('click', openSidebarSearch);
  $('view-options').addEventListener('click', event => openViewOptions(event.currentTarget));
  $('add-workspace').addEventListener('click', () => action(async () => {
    const folder = await api.chooseFolder();
    if (folder) selectWorkspace(folder);
  }));
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
    if ((event.metaKey || event.ctrlKey) && !event.shiftKey && event.key.toLowerCase() === 'n') { event.preventDefault(); newSession(); }
    if ((event.metaKey || event.ctrlKey) && event.key === ',') { event.preventDefault(); openSettings('general'); }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'f' && current) { event.preventDefault(); openFind(); }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 't' && !event.altKey) { event.preventDefault(); openRightbarTab('browser'); }
    if ((event.metaKey || event.ctrlKey) && event.altKey && event.key.toLowerCase() === 'b') { event.preventDefault(); rightbarOpen ? closeRightbar() : openRightbar(); }
    if ((event.metaKey || event.ctrlKey) && !event.altKey && event.key.toLowerCase() === 'b') { event.preventDefault(); toggleSidebar(); }
    if (event.key === 'Escape' && document.body.classList.contains('rightbar-expanded')) { event.preventDefault(); toggleRightbarFullscreen(); }
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
  if ((!content && !refs.length) || loading || uncertain || uploadPending()) return;
  const unsupported = attachmentSubmissionError(refs);
  if (unsupported) { composerError(unsupported); return; }
  composerError('');
  action(async () => {
    if (!current) await createTask();
    const running = ['running', 'waiting_user'].includes(current.status);
    const steer = running && !!content && (complement ? settings.busySend !== 'steer' : settings.busySend === 'steer');
    if (steer) {
      await deliver({ type: 'steer_current_turn', session_id: current.session.id, content });
    } else {
      await deliver({ type: 'submit_message', session_id: current.session.id, content, attachments: refs });
    }
    if ($('message').value.trim() === content) { $('message').value = ''; resizeComposer(); }
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
