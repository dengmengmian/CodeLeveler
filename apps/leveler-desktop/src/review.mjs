// Review surface projection.
//
// The runtime owns review execution, durability and the report. This module
// only turns those typed facts into the labels, options and card models the
// Desktop paints. It is pure: no DOM, no transport, no timers.
//
// Two rules come from the runtime contract and must not be softened here:
// - A review is started only by an explicit user action. Nothing in this
//   module classifies a message or decides that a review should happen.
// - An absent conclusion/coverage/limitation is "not declared", never
//   "no issues". `no_issues_found` is the reviewer's own statement about the
//   reviewed object, never a proof that the project is correct.

/** The runtime's focus bound (`FOCUS_MAX_BYTES` in the review service). */
export const REVIEW_FOCUS_MAX_BYTES = 8 * 1024;
/** The runtime's path-list bounds. */
export const REVIEW_PATHS_MAX = 50;
export const REVIEW_PATH_MAX_BYTES = 500;

/**
 * UTF-8 byte length without relying on a host global. The runtime bounds the
 * focus in bytes, so the composer must count the same unit a Chinese character
 * spends three of.
 */
export function reviewFocusBytes(value) {
  let bytes = 0;
  for (const char of String(value ?? '')) {
    const code = char.codePointAt(0);
    bytes += code <= 0x7f ? 1 : code <= 0x7ff ? 2 : code <= 0xffff ? 3 : 4;
  }
  return bytes;
}
const bytesOf = reviewFocusBytes;

const STATUS_VIEW = {
  queued: { label: '排队中', tone: 'muted', settled: false },
  running: { label: '审查中', tone: 'run', settled: false },
  completed: { label: '已完成', tone: 'ok', settled: true },
  incomplete: { label: '未完成', tone: 'warn', settled: true },
  cancelled: { label: '已取消', tone: 'muted', settled: true },
  failed: { label: '失败', tone: 'danger', settled: true },
};

/** Status label. An unknown wire value is shown as itself, never coerced. */
export function reviewStatusView(status) {
  return STATUS_VIEW[status] ?? { label: String(status ?? 'unknown'), tone: 'muted', settled: false };
}

const SCOPE_LABEL = {
  auto: '自动选择',
  current_task: '当前任务',
  workspace: '工作区变更',
  project: '整个项目',
  session: '当前会话',
  commit: '单个提交',
  branch: '分支对比',
  paths: '指定路径',
};

export function reviewScopeKindLabel(kind) {
  return SCOPE_LABEL[kind] ?? String(kind ?? '未知范围');
}

/** One-line scope identity, including the object the request named. */
export function reviewScopeText(scope) {
  if (!scope) return '未知范围';
  switch (scope.kind) {
    case 'commit': return `提交 ${scope.rev}`;
    case 'branch': return `相对 ${scope.base} 的分支变更`;
    case 'paths': return `指定路径：${(scope.paths ?? []).join('、')}`;
    case 'current_task': return '当前任务（需求、交付与证据）';
    case 'session': return '当前会话的执行与交付记录';
    case 'workspace': return '工作区未提交变更';
    case 'project': return '整个项目';
    case 'auto': return '自动选择审查对象';
    default: return String(scope.kind ?? '未知范围');
  }
}

/** What the reviewer will actually be asked to look at, for the dialog. */
export function reviewScopeDetail(scope) {
  switch (scope?.kind) {
    case 'workspace':
      return '包含已暂存、未暂存和未跟踪的文件；基线是 HEAD，比较的是当前工作区状态。';
    case 'commit':
      return '只读取该提交的对象与祖先历史；工具不能代表工作区里的其他改动。';
    case 'branch':
      return '比较 `git diff base...HEAD`；基线只使用你指定的引用，不会联网抓取。';
    case 'paths':
      return '只审查列出的路径；路径必须是工作区内的相对路径。';
    case 'project':
      return '审查者读取仓库本身，范围由你输入的审查重点决定。';
    case 'current_task':
      return '读取本会话最近一次交付：用户目标、交付声明、被改动的文件和需求记录。';
    case 'session':
      return '读取本会话的用户消息、任务终态、工具执行与交付记录。';
    default:
      return '';
  }
}

const CONCLUSION_VIEW = {
  issues_found: {
    label: '发现问题',
    tone: 'danger',
    note: '审查者声明在该范围内发现问题；具体问题以正式 Finding 为准。',
  },
  no_issues_found: {
    label: '未发现问题',
    tone: 'ok',
    note: '只表示审查者在其声明的覆盖范围内没有发现问题，不代表项目已被证明正确或安全。',
  },
  inconclusive: {
    label: '证据不足',
    tone: 'warn',
    note: '审查者无法在已有证据上定论；缺失的内容见覆盖情况与限制。',
  },
};

/** The reviewer's verdict. `null`/absent stays "未声明", never "通过". */
export function reviewConclusionView(conclusion) {
  return CONCLUSION_VIEW[conclusion] ?? {
    label: '结论未声明',
    tone: 'muted',
    note: '审查者没有声明总体结论；这不等于没有问题。',
  };
}

export function reviewSeverityLabel(severity) {
  return { p0: 'P0 · 严重', p1: 'P1 · 高', p2: 'P2 · 中', p3: 'P3 · 低' }[severity] ?? (severity ? String(severity).toUpperCase() : '未分级');
}

export function reviewSeverityTone(severity) {
  return { p0: 'danger', p1: 'danger', p2: 'warn', p3: 'muted' }[severity] ?? 'muted';
}

export function reviewDimensionLabel(dimension) {
  return {
    code: '代码', business: '业务', security: '安全', architecture: '架构',
    test: '测试', delivery: '交付', session: '会话',
  }[dimension] ?? (dimension ? String(dimension) : '未标注维度');
}

export function reviewCoverageLabel(status) {
  return {
    reviewed: '已审查', partial: '部分审查',
    not_reviewed: '未审查', not_applicable: '不适用',
  }[status] ?? String(status ?? '未声明');
}

export function reviewCoverageTone(status) {
  return { reviewed: 'ok', partial: 'warn', not_reviewed: 'muted', not_applicable: 'muted' }[status] ?? 'muted';
}

export function reviewLimitationKindLabel(kind) {
  return {
    environment: '环境', permission: '权限', tool_capability: '工具能力',
    scope: '范围', object_changed: '对象变化', other: '其他',
  }[kind] ?? String(kind ?? '未知');
}

export function reviewLimitationSourceLabel(source) {
  return { runtime: '运行时', reviewer: '审查者' }[source] ?? String(source ?? '未标注来源');
}

export function reviewFindingDispositionLabel(disposition) {
  return {
    candidate: '候选', adopted: '已采纳', rejected: '已否定', duplicate: '重复',
  }[disposition] ?? '正式';
}

/** Counts by severity. `unclassified` and candidates stay visible. */
export function reviewCountsText(counts) {
  if (!counts) return '未统计';
  const parts = [];
  for (const severity of ['p0', 'p1', 'p2', 'p3']) if (counts[severity]) parts.push(`${severity.toUpperCase()} ${counts[severity]}`);
  if (counts.unclassified) parts.push(`未分级 ${counts.unclassified}`);
  if (counts.observations) parts.push(`其中观察 ${counts.observations}`);
  // The runtime's own formal total: P0-P3 plus unclassified. Candidates are
  // reported separately and never inflate it.
  const total = (counts.p0 ?? 0) + (counts.p1 ?? 0) + (counts.p2 ?? 0) + (counts.p3 ?? 0) + (counts.unclassified ?? 0);
  parts.push(`共 ${total} 条`);
  if (counts.pending_candidates) parts.push(`候选 ${counts.pending_candidates}`);
  return parts.join(' · ');
}

export function reviewUsageText(usage) {
  if (!usage) return '';
  return `输入 ${usage.input_tokens ?? 0} · 输出 ${usage.output_tokens ?? 0}${usage.cached_input_tokens ? ` · 缓存 ${usage.cached_input_tokens}` : ''}`;
}

/** The card model for one review, with live progress layered on when running. */
export function reviewCardModel(summary, live = null) {
  if (!summary) return null;
  const status = reviewStatusView(summary.status);
  const conclusion = summary.conclusion ? reviewConclusionView(summary.conclusion) : null;
  return {
    id: summary.id,
    status,
    scope: summary.scope,
    scopeLabel: reviewScopeKindLabel(summary.scope?.kind),
    scopeText: reviewScopeText(summary.scope),
    model: summary.model ?? '',
    focus: summary.focus ?? '',
    countsText: reviewCountsText(summary.counts),
    conclusion,
    usageText: reviewUsageText(summary.usage),
    createdAt: summary.created_at ?? '',
    finishedAt: summary.finished_at ?? null,
    failure: summary.failure ?? null,
    stopDetail: summary.stop_detail ?? null,
    objectChanged: summary.object_changed === true,
    modifiedSource: summary.modified_source === true,
    reportStale: summary.report_stale === true,
    reportSource: summary.report_source ?? 'legacy',
    hasFormalReport: (summary.report_source ?? 'legacy') !== 'none',
    canCancel: !status.settled,
    investigations: (summary.investigations ?? []).map(item => ({
      id: item.id,
      title: item.title ?? '',
      role: item.role ?? '',
      state: item.state ?? '',
      findings: item.findings ?? 0,
      elapsedMs: item.elapsed_ms ?? 0,
    })),
    live: status.settled ? null : {
      active: live?.active === true,
      tool: live?.tool ?? null,
      elapsedMs: live?.elapsedMs ?? 0,
      inputTokens: live?.inputTokens ?? 0,
      outputTokens: live?.outputTokens ?? 0,
      cachedInputTokens: live?.cachedInputTokens ?? 0,
    },
  };
}

/**
 * The scope picker's options, built from the runtime's own Git facts.
 *
 * `enabled` and `reason` are facts about the object, not guesses: a clean
 * workspace has nothing for the workspace scope to review, and a branch with
 * no recorded upstream cannot be compared against one the runtime did not
 * choose. The runtime revalidates every choice again.
 */
export function reviewScopeOptions(facts = {}) {
  const hasWorkspace = facts.workspace === true;
  const git = facts.git === true;
  const upstream = facts.upstream ?? null;
  const ahead = Number(facts.ahead ?? 0);
  const recommended = facts.recommended?.kind ?? null;
  const option = (id, group, label, description, scope, enabled, reason) => ({
    id, group, label, description, scope, enabled, reason,
    recommended: recommended === id,
  });
  const noWorkspace = '当前会话的工作区无法被这个运行时读取';
  return [
    option(
      'current_task', '交付与业务', '当前任务',
      '审查本会话最近一次交付是否满足原始需求、证据是否充分。',
      { kind: 'current_task' }, hasWorkspace,
      hasWorkspace ? null : noWorkspace,
    ),
    option(
      'session', '交付与业务', '当前会话执行过程',
      '审查本会话的任务、计划、工具执行与交付记录，而不只看代码。',
      { kind: 'session' }, hasWorkspace,
      hasWorkspace ? null : noWorkspace,
    ),
    option(
      'workspace', '代码与仓库', '工作区未提交变更',
      '审查已暂存、未暂存和未跟踪的改动。',
      { kind: 'workspace' }, hasWorkspace && git && facts.dirty === true,
      !hasWorkspace ? noWorkspace : !git ? '无法读取 Git 状态，不能确定工作区变更' : facts.dirty !== true ? '工作区没有未提交变更' : null,
    ),
    option(
      'branch', '代码与仓库', upstream ? `相对 ${upstream} 的分支` : '分支相对上游',
      '审查当前分支相对本地记录的远程跟踪引用领先的提交。',
      upstream ? { kind: 'branch', base: upstream } : { kind: 'branch', base: '' },
      git && !!upstream && ahead > 0,
      !git ? '无法读取 Git 状态' : !upstream ? '当前分支没有记录的上游' : ahead <= 0 ? '当前分支没有领先上游的提交' : null,
    ),
    option(
      'commit', '代码与仓库', '单个提交',
      '审查一个提交及其祖先历史。',
      { kind: 'commit', rev: '' }, git,
      git ? null : '无法读取 Git 状态',
    ),
    option(
      'paths', '代码与仓库', '指定路径',
      '只审查工作区内的指定文件或目录（逗号分隔）。',
      { kind: 'paths', paths: [] }, hasWorkspace,
      hasWorkspace ? null : noWorkspace,
    ),
    option(
      'project', '代码与仓库', '整个项目',
      '审查者读取仓库本身；范围由审查重点决定。',
      { kind: 'project' }, hasWorkspace,
      hasWorkspace ? null : noWorkspace,
    ),
  ];
}

/** The option the picker preselects: only the runtime's own recommendation. */
export function recommendedScopeOption(options) {
  return options.find(option => option.recommended && option.enabled)?.id ?? null;
}

/** Split the paths field the way the dialog's hint describes. */
export function reviewPathsFromText(text) {
  return String(text ?? '').split(/[,\n]/).map(part => part.trim()).filter(Boolean);
}

/**
 * Build the wire scope from a picker selection.
 *
 * Throws a Chinese reason the dialog can show verbatim; nothing is started
 * until `reviewRequest` returns.
 */
export function reviewScopeFromSelection(selection = {}) {
  const kind = selection.kind;
  switch (kind) {
    case 'current_task':
    case 'session':
    case 'workspace':
    case 'project':
      return { kind };
    case 'commit': {
      const rev = String(selection.rev ?? '').trim();
      if (!rev) throw new Error('请输入要审查的提交（例如 HEAD~1 或完整 SHA）');
      if (rev.length > 200 || rev.startsWith('-') || /[\u0000-\u001f\u007f]/.test(rev)) throw new Error('无效的提交引用');
      return { kind: 'commit', rev };
    }
    case 'branch': {
      const base = String(selection.base ?? '').trim();
      if (!base) throw new Error('请输入比较基线（例如 origin/main）');
      if (base.length > 200 || base.startsWith('-') || /[\u0000-\u001f\u007f]/.test(base)) throw new Error('无效的比较基线');
      return { kind: 'branch', base };
    }
    case 'paths': {
      const paths = reviewPathsFromText(selection.pathsText ?? (selection.paths ?? []).join(','));
      if (!paths.length) throw new Error('请至少输入一个路径');
      if (paths.length > REVIEW_PATHS_MAX) throw new Error(`路径最多 ${REVIEW_PATHS_MAX} 个`);
      for (const path of paths) {
        if (path.startsWith('/') || path.startsWith('\\') || path.includes('\0') || path.split('/').includes('..')) throw new Error(`路径必须是工作区内的相对路径：${path}`);
        if (bytesOf(path) > REVIEW_PATH_MAX_BYTES) throw new Error(`路径过长：${path}`);
      }
      return { kind: 'paths', paths };
    }
    default:
      throw new Error('请选择审查对象');
  }
}

/**
 * The `run_review` command for one explicit user action.
 *
 * `focus` may be empty: the runtime then records "全面审查所选范围". It is the
 * user's own words, never a generated instruction.
 */
export function reviewRequest({ sessionId, focus = '', scope }) {
  if (!sessionId) throw new Error('没有选中的会话');
  if (!scope?.kind) throw new Error('请选择审查对象');
  if (bytesOf(focus) > REVIEW_FOCUS_MAX_BYTES) throw new Error('审查重点过长（上限 8 KiB）');
  return { type: 'run_review', session_id: sessionId, focus, scope };
}

/** Why the entry cannot start a review right now, if anything. */
export function reviewEntryState({ session, status, uncertain, submitting } = {}) {
  if (!session) return { enabled: false, reason: '请先选择或新建会话' };
  if (uncertain) return { enabled: false, reason: '上一个操作结果未知，请先刷新确认' };
  if (submitting) return { enabled: false, reason: '正在提交审查请求' };
  if (status === 'running' || status === 'waiting_user') {
    return { enabled: false, reason: '当前回合正在运行；等它结束或取消后再审查（审查不会中断或排队）' };
  }
  return { enabled: true, reason: null };
}

/** One finding as the detail view paints it. */
export function reviewFindingView(finding) {
  return {
    id: finding.id,
    title: finding.title || finding.summary || finding.id,
    summary: finding.summary ?? '',
    severity: finding.severity ? { label: reviewSeverityLabel(finding.severity), tone: reviewSeverityTone(finding.severity) } : null,
    dimension: finding.dimension ? reviewDimensionLabel(finding.dimension) : null,
    confidence: finding.confidence ? { high: '高置信度', medium: '中等置信度', low: '低置信度' }[finding.confidence] ?? finding.confidence : null,
    kind: finding.kind ?? '',
    file: finding.file ?? null,
    line: Number.isInteger(finding.line) ? finding.line : null,
    symbol: finding.symbol ?? null,
    evidence: finding.evidence ?? null,
    impact: finding.impact ?? null,
    recommendation: finding.recommendation ?? null,
    reproduced: finding.reproduced === true,
    disposition: finding.disposition ?? null,
    dispositionLabel: reviewFindingDispositionLabel(finding.disposition),
    dispositionReason: finding.disposition_reason ?? null,
    duplicateOf: finding.duplicate_of ?? null,
    sourceChild: finding.source_child ?? '',
    formal: finding.disposition === null || finding.disposition === undefined || finding.disposition === 'adopted',
  };
}

/**
 * The full report as the detail view paints it.
 *
 * The structured verdict comes first; the reviewer's narrative is shown as
 * the reviewer wrote it and is never parsed for control flow.
 */
export function reviewReportView(report) {
  if (!report) return null;
  const summary = report.summary;
  const findings = (report.findings ?? []).map(reviewFindingView);
  const formal = findings.filter(finding => finding.formal);
  const decided = findings.filter(finding => !finding.formal);
  const verdict = report.verdict ?? {};
  return {
    summary: reviewCardModel(summary),
    object: {
      workspace: report.object?.workspace ?? null,
      head: report.object?.head ?? null,
      dirty: report.object?.dirty === true,
      revision: report.object?.revision ?? null,
      files: report.object?.files ?? [],
      untracked: report.object?.untracked ?? [],
      capturedAt: report.object?.captured_at ?? '',
      fingerprint: report.object?.fingerprint ?? '',
    },
    conclusion: verdict.conclusion ? reviewConclusionView(verdict.conclusion) : null,
    coverage: (verdict.coverage ?? []).map(item => ({
      dimension: reviewDimensionLabel(item.dimension),
      status: reviewCoverageLabel(item.status),
      tone: reviewCoverageTone(item.status),
      scope: item.scope ?? null,
      evidenceRefs: item.evidence_refs ?? [],
      limitations: item.limitations ?? null,
    })),
    limitations: (verdict.limitations ?? []).map(item => ({
      source: reviewLimitationSourceLabel(item.source),
      kind: reviewLimitationKindLabel(item.kind),
      detail: item.detail,
    })),
    formal,
    decided,
    findingsDeclared: findings.length > 0,
    reportText: report.report ?? '',
    finalMessage: report.final_message ?? '',
  };
}

/** The live progress payload a card shows while a review runs. */
export function reviewLiveFromEvent(event) {
  return {
    active: event.active === true,
    tool: event.tool ?? null,
    elapsedMs: event.elapsed_ms ?? 0,
    inputTokens: event.input_tokens ?? 0,
    outputTokens: event.output_tokens ?? 0,
    cachedInputTokens: event.cached_input_tokens ?? 0,
  };
}

/**
 * Apply one review runtime event to the conversation state.
 *
 * Cards are keyed by the durable review id. A settlement may create its own
 * card (a client that missed the start still gets the result), but a progress
 * event never invents one: progress carries no identity beyond an id, and a
 * card without its durable summary would be a second truth.
 */
export function applyReviewEvent(state, event) {
  if (typeof event?.type !== 'string' || !event.type.startsWith('review_')) return null;
  const reviews = () => (state.reviews ?? []).map(row => ({ ...row, summary: row.summary ? { ...row.summary } : null }));
  const index = rows => rows.findIndex(row => row.id === (event.review?.id ?? event.review_id ?? event.report?.summary?.id));
  const append = (rows, card) => {
    const seq = Number.isFinite(event.seq) ? event.seq : (state.nextSeq ?? rows.length);
    rows.push({ ...card, seq });
    return Math.max(state.nextSeq ?? 0, seq + 1);
  };
  switch (event.type) {
    case 'review_started': {
      const rows = reviews();
      const at = index(rows);
      const summary = event.review;
      if (!summary?.id) return state;
      let nextSeq = state.nextSeq ?? 0;
      if (at >= 0) rows[at] = { ...rows[at], summary };
      else nextSeq = append(rows, { id: summary.id, summary, report: null, live: null });
      return { ...state, reviews: rows, nextSeq };
    }
    case 'review_progress': {
      const rows = reviews();
      const at = rows.findIndex(row => row.id === event.review_id);
      if (at < 0) return state;
      rows[at] = { ...rows[at], live: reviewLiveFromEvent(event) };
      return { ...state, reviews: rows };
    }
    case 'review_finished': {
      const rows = reviews();
      const summary = event.review;
      if (!summary?.id) return state;
      const at = rows.findIndex(row => row.id === summary.id);
      const report = event.report ?? (at >= 0 ? rows[at].report : null);
      let nextSeq = state.nextSeq ?? 0;
      if (at >= 0) rows[at] = { ...rows[at], summary, report, live: null };
      else nextSeq = append(rows, { id: summary.id, summary, report, live: null });
      return { ...state, reviews: rows, nextSeq };
    }
    case 'review_report_loaded': {
      const rows = reviews();
      const report = event.report;
      const id = report?.summary?.id;
      if (!id) return state;
      const at = rows.findIndex(row => row.id === id);
      let nextSeq = state.nextSeq ?? 0;
      if (at >= 0) rows[at] = { ...rows[at], summary: report.summary, report };
      else nextSeq = append(rows, { id, summary: report.summary, report, live: null });
      return { ...state, reviews: rows, nextSeq };
    }
    default:
      return null;
  }
}

/**
 * Reconcile the runtime's durable review list into the conversation.
 *
 * The list is an index, not a transcript: a review the conversation already
 * carries keeps its place and only takes the fresher durable summary. A
 * review it does not carry (written before the conversation-notice mechanism,
 * or settled while this client was away) is appended once, in created order.
 */
export function reconcileReviewHistory(state, event) {
  if (event?.type !== 'review_history_loaded') return state;
  const rows = (state.reviews ?? []).map(row => ({ ...row, summary: row.summary ? { ...row.summary } : null }));
  const reports = new Map((event.reports ?? []).map(report => [report.summary.id, report]));
  let nextSeq = state.nextSeq ?? 0;
  const missing = [];
  for (const summary of event.reviews ?? []) {
    const at = rows.findIndex(row => row.id === summary.id);
    if (at >= 0) {
      const existing = rows[at];
      rows[at] = {
        ...existing,
        summary,
        // A settled inline report is the durable result; it replaces a card
        // that only knew the start. A live progress snapshot is stale once a
        // durable terminal exists.
        report: existing.report ?? reports.get(summary.id) ?? null,
        live: reviewStatusView(summary.status).settled ? null : existing.live,
      };
    } else {
      missing.push({ summary, report: reports.get(summary.id) ?? null });
    }
  }
  missing.sort((left, right) => String(left.summary.created_at ?? '').localeCompare(String(right.summary.created_at ?? '')));
  for (const item of missing) {
    rows.push({ id: item.summary.id, seq: nextSeq++, summary: item.summary, report: item.report, live: null });
  }
  return { ...state, reviews: rows, nextSeq };
}
