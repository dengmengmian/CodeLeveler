import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  reviewStatusView, reviewScopeText, reviewConclusionView, reviewCountsText,
  reviewCardModel, reviewScopeOptions, recommendedScopeOption, reviewScopeFromSelection,
  reviewRequest, reviewEntryState, reviewReportView, applyReviewEvent,
  reconcileReviewHistory, reviewFocusBytes, REVIEW_FOCUS_MAX_BYTES, reviewPathsFromText,
} from '../src/review.mjs';
import { projectSnapshot, applyEvent, mergeRestoredConversation } from '../src/state.mjs';
import { conversationItems } from '../src/presentation.mjs';

const summary = (over = {}) => ({
  id: 'review-1',
  session_id: 'session-1',
  focus: '检查这份改动',
  scope: { kind: 'workspace' },
  model: 'fixture/m',
  status: 'running',
  created_at: '2026-11-27T10:00:00Z',
  counts: { p0: 0, p1: 0, p2: 0, p3: 0, unclassified: 0, pending_candidates: 0, observations: 0 },
  object_changed: false,
  modified_source: false,
  usage: { input_tokens: 12, output_tokens: 3, cached_input_tokens: 0 },
  report_source: 'none',
  report_stale: false,
  ...over,
});

const report = (over = {}) => ({
  summary: summary({ status: 'completed', report_source: 'submitted', conclusion: 'issues_found' }),
  object: { workspace: '/tmp/ws', head: '0123456789abcdef', dirty: true, files: ['src/lib.rs'], untracked: [], captured_at: '2026-11-27T10:00:00Z' },
  verdict: {
    conclusion: 'issues_found',
    coverage: [{ dimension: 'business', status: 'partial', evidence_refs: ['session_history'] }],
    limitations: [{ source: 'runtime', kind: 'tool_capability', detail: '只读工具，不能运行测试' }],
  },
  findings: [
    { id: 'f-1', title: '边界判断错误', summary: '空输入被当成 0', severity: 'p1', dimension: 'code', confidence: 'high', kind: 'correctness', disposition: null, file: 'src/lib.rs', line: 42, evidence: 'if (x == null) return 0', impact: '错误结果', recommendation: '显式拒绝', reproduced: false },
    { id: 'f-2', title: '重复实现', summary: '与 utils 重复', severity: 'p3', dimension: 'architecture', disposition: 'duplicate', duplicate_of: 'f-1' },
    { id: 'f-3', title: '候选问题', summary: '可能为空', disposition: 'candidate', severity: 'p2' },
  ],
  report: '## 结论\n\n存在一个边界问题。',
  final_message: '',
  ...over,
});

test('every review status and conclusion has an honest label', () => {
  assert.deepEqual(reviewStatusView('running'), { label: '审查中', tone: 'run', settled: false });
  assert.equal(reviewStatusView('completed').settled, true);
  assert.equal(reviewStatusView('incomplete').tone, 'warn');
  assert.equal(reviewStatusView('failed').tone, 'danger');
  // An unknown wire value is shown as itself, never coerced into a success.
  assert.equal(reviewStatusView('mystery').label, 'mystery');
  assert.equal(reviewStatusView('mystery').settled, false);
  // "No issues found" is the reviewer's own statement; it is never a proof.
  const clean = reviewConclusionView('no_issues_found');
  assert.equal(clean.label, '未发现问题');
  assert.match(clean.note, /不代表项目已被证明正确或安全/);
  // An absent conclusion is not a pass.
  assert.equal(reviewConclusionView(null).label, '结论未声明');
  assert.match(reviewConclusionView(null).note, /不等于没有问题/);
  assert.equal(reviewConclusionView('inconclusive').label, '证据不足');
});

test('counts never promote candidates and never hide unclassified findings', () => {
  assert.equal(
    reviewCountsText({ p0: 1, p1: 0, p2: 2, p3: 0, unclassified: 1, observations: 1, pending_candidates: 3 }),
    'P0 1 · P2 2 · 未分级 1 · 其中观察 1 · 共 4 条 · 候选 3',
  );
  assert.equal(
    reviewCountsText({ p0: 0, p1: 0, p2: 0, p3: 0, unclassified: 0, pending_candidates: 2 }),
    '共 0 条 · 候选 2',
  );
});

test('the scope picker offers only objects the runtime facts support', () => {
  const options = reviewScopeOptions({
    workspace: true, git: true, dirty: true, branch: 'main', upstream: 'origin/main', ahead: 2,
    recommended: { kind: 'workspace' },
  });
  const byId = new Map(options.map(option => [option.id, option]));
  assert.equal(byId.get('workspace').enabled, true);
  assert.equal(byId.get('workspace').recommended, true);
  assert.equal(byId.get('branch').enabled, true);
  assert.deepEqual(byId.get('branch').scope, { kind: 'branch', base: 'origin/main' });
  assert.equal(byId.get('current_task').enabled, true);
  assert.equal(recommendedScopeOption(options), 'workspace');

  const clean = reviewScopeOptions({ workspace: true, git: true, dirty: false, upstream: null, ahead: 0 });
  const cleanById = new Map(clean.map(option => [option.id, option]));
  assert.equal(cleanById.get('workspace').enabled, false);
  assert.match(cleanById.get('workspace').reason, /没有未提交变更/);
  assert.equal(cleanById.get('branch').enabled, false);
  assert.equal(cleanById.get('project').enabled, true);
  assert.equal(recommendedScopeOption(clean), null);

  const noWorkspace = reviewScopeOptions({ workspace: false, git: false, dirty: false });
  assert.equal(noWorkspace.every(option => !option.enabled), true);
  assert.match(noWorkspace[0].reason, /无法被这个运行时读取/);

  // A branch with a recorded upstream but nothing ahead is not a default.
  const behind = reviewScopeOptions({ workspace: true, git: true, dirty: false, upstream: 'origin/main', ahead: 0, recommended: null });
  assert.equal(behind.find(option => option.id === 'branch').recommended, false);
});

test('a chosen scope becomes the exact wire object the runtime documents', () => {
  assert.deepEqual(reviewScopeFromSelection({ kind: 'workspace' }), { kind: 'workspace' });
  assert.deepEqual(reviewScopeFromSelection({ kind: 'session' }), { kind: 'session' });
  assert.deepEqual(reviewScopeFromSelection({ kind: 'commit', rev: ' HEAD~1 ' }), { kind: 'commit', rev: 'HEAD~1' });
  assert.deepEqual(reviewScopeFromSelection({ kind: 'branch', base: 'origin/main' }), { kind: 'branch', base: 'origin/main' });
  assert.deepEqual(
    reviewScopeFromSelection({ kind: 'paths', pathsText: 'src/a.rs,\n crates/b ' }),
    { kind: 'paths', paths: ['src/a.rs', 'crates/b'] },
  );
  assert.throws(() => reviewScopeFromSelection({ kind: 'commit', rev: '--upload-pack=x' }), /无效的提交引用/);
  assert.throws(() => reviewScopeFromSelection({ kind: 'commit', rev: '' }), /请输入/);
  assert.throws(() => reviewScopeFromSelection({ kind: 'paths', pathsText: '../etc/passwd' }), /相对路径/);
  assert.throws(() => reviewScopeFromSelection({ kind: 'paths', pathsText: '/etc/passwd' }), /相对路径/);
  assert.throws(() => reviewScopeFromSelection({ kind: 'paths', pathsText: '' }), /至少输入一个路径/);
  assert.throws(() => reviewScopeFromSelection({ kind: 'nope' }), /请选择审查对象/);
  assert.deepEqual(reviewPathsFromText('a,,b\nc'), ['a', 'b', 'c']);
});

test('the request carries the user words verbatim and enforces the runtime byte bound', () => {
  assert.deepEqual(
    reviewRequest({ sessionId: 's1', focus: '只看并发', scope: { kind: 'workspace' } }),
    { type: 'run_review', session_id: 's1', focus: '只看并发', scope: { kind: 'workspace' } },
  );
  // An empty focus is allowed: the runtime substitutes its own neutral default.
  assert.equal(reviewRequest({ sessionId: 's1', focus: '', scope: { kind: 'workspace' } }).focus, '');
  assert.throws(() => reviewRequest({ sessionId: '', scope: { kind: 'workspace' } }), /没有选中的会话/);
  assert.throws(() => reviewRequest({ sessionId: 's1' }), /请选择审查对象/);
  // The bound is UTF-8 bytes, the unit the runtime uses.
  const multibyte = '中'.repeat(REVIEW_FOCUS_MAX_BYTES);
  assert.ok(reviewFocusBytes(multibyte) > REVIEW_FOCUS_MAX_BYTES);
  assert.throws(() => reviewRequest({ sessionId: 's1', focus: multibyte, scope: { kind: 'workspace' } }), /过长/);
});

test('the entry is disabled for a running turn and an unknown outcome, never silently queued', () => {
  assert.equal(reviewEntryState({ session: null }).enabled, false);
  assert.match(reviewEntryState({ session: { id: 's' }, status: 'running' }).reason, /正在运行/);
  assert.match(reviewEntryState({ session: { id: 's' }, status: 'waiting_user' }).reason, /正在运行/);
  assert.match(reviewEntryState({ session: { id: 's' }, status: 'idle', uncertain: { command_id: 'x' } }).reason, /结果未知/);
  assert.equal(reviewEntryState({ session: { id: 's' }, status: 'idle' }).enabled, true);
});

test('a settled card never claims a report it does not have', () => {
  const running = reviewCardModel(summary(), { active: true, tool: 'read_file', elapsedMs: 1500, inputTokens: 10, outputTokens: 2 });
  assert.equal(running.canCancel, true);
  assert.equal(running.live.tool, 'read_file');
  assert.equal(running.hasFormalReport, false);

  const none = reviewCardModel(summary({ status: 'completed', report_source: 'none' }));
  assert.equal(none.canCancel, false);
  assert.equal(none.hasFormalReport, false);
  assert.equal(none.live, null);

  const legacy = reviewCardModel(summary({ status: 'completed', report_source: 'legacy' }));
  assert.equal(legacy.hasFormalReport, true);

  const broken = reviewCardModel(summary({ status: 'failed', failure: '模型不可用' }));
  assert.equal(broken.failure, '模型不可用');
  assert.equal(broken.canCancel, false);
});

test('the report keeps formal findings, dispositions and limitations apart', () => {
  const view = reviewReportView(report());
  assert.deepEqual(view.formal.map(finding => finding.id), ['f-1']);
  assert.deepEqual(view.decided.map(finding => finding.id), ['f-2', 'f-3']);
  assert.equal(view.formal[0].file, 'src/lib.rs');
  assert.equal(view.formal[0].line, 42);
  assert.equal(view.formal[0].severity.label, 'P1 · 高');
  assert.equal(view.conclusion.label, '发现问题');
  assert.deepEqual(view.coverage.map(item => [item.dimension, item.status]), [['业务', '部分审查']]);
  assert.deepEqual(view.limitations.map(item => [item.source, item.kind]), [['运行时', '工具能力']]);
  assert.equal(view.summary.scopeText, '工作区未提交变更');
  assert.match(view.object.workspace, /\/tmp\/ws/);
});

test('review events build one card per durable id and never invent progress', () => {
  let state = projectSnapshot({ id: 'session-1', messages: [], task_status: 'idle', pending_interactions: [], active_tools: [] });
  // Progress without a start is not a card: it carries no durable summary.
  state = applyEvent(state, { type: 'review_progress', review_id: 'review-1', active: true, input_tokens: 1 });
  assert.equal(state.reviews.length, 0);
  state = applyEvent(state, { type: 'review_started', review: summary() });
  assert.equal(state.reviews.length, 1);
  const seq = state.reviews[0].seq;
  state = applyEvent(state, { type: 'review_progress', review_id: 'review-1', active: true, tool: 'grep', elapsed_ms: 900, input_tokens: 5, output_tokens: 1 });
  assert.equal(state.reviews[0].live.tool, 'grep');
  assert.equal(state.reviews[0].seq, seq);
  // A settlement with the report converges onto the same card.
  state = applyEvent(state, { type: 'review_finished', review: summary({ status: 'completed', report_source: 'submitted' }), report: report() });
  assert.equal(state.reviews.length, 1);
  assert.equal(state.reviews[0].live, null);
  assert.equal(state.reviews[0].report.findings.length, 3);
  // A duplicate terminal does not create a second card.
  state = applyEvent(state, { type: 'review_finished', review: summary({ status: 'completed', report_source: 'submitted' }) });
  assert.equal(state.reviews.length, 1);
  // A settlement whose start was missed still becomes one card.
  state = applyEvent(state, { type: 'review_finished', review: summary({ id: 'review-2', status: 'failed', failure: 'x' }) });
  assert.deepEqual(state.reviews.map(review => review.id), ['review-1', 'review-2']);
});

test('the durable list merges by id and appends only reviews the conversation lacks', () => {
  let state = projectSnapshot({ id: 'session-1', messages: [], task_status: 'idle', pending_interactions: [], active_tools: [] });
  state = applyEvent(state, { type: 'review_started', review: summary({ id: 'review-old', scope: { kind: 'project' } }) });
  const seq = state.reviews[0].seq;
  const history = {
    type: 'review_history_loaded',
    reviews: [
      summary({ id: 'review-new', status: 'completed', created_at: '2026-11-27T11:00:00Z' }),
      summary({ id: 'review-old', status: 'incomplete', created_at: '2026-11-27T09:00:00Z' }),
    ],
    reports: [report({ summary: summary({ id: 'review-new', status: 'completed', report_source: 'submitted' }) })],
    delivered: ['review-old'],
  };
  state = applyEvent(state, history);
  assert.deepEqual(state.reviews.map(review => review.id), ['review-old', 'review-new']);
  // The existing card keeps its place and takes the newer durable summary.
  assert.equal(state.reviews[0].seq, seq);
  assert.equal(state.reviews[0].summary.status, 'incomplete');
  assert.equal(state.reviews[0].live, null);
  // The missing review is appended with its inline report.
  assert.equal(state.reviews[1].report.findings.length, 3);
  // Replaying the same list is idempotent.
  state = applyEvent(state, history);
  assert.deepEqual(state.reviews.map(review => review.id), ['review-old', 'review-new']);
});

test('a reopened conversation carries review cards in log order', () => {
  const history = [
    { event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: '/review @workspace 检查' } } },
    { event: { type: 'review_started', review: summary() } },
    { event: { type: 'review_finished', review: summary({ status: 'completed', report_source: 'submitted', conclusion: 'no_issues_found' }), report: report() } },
    { event: { type: 'user_message_added', message: { id: 'u2', role: 'user', text: '继续' } } },
  ];
  const state = projectSnapshot({ id: 'session-1', messages: [], task_status: 'idle', pending_interactions: [], active_tools: [] }, history);
  assert.equal(state.reviews.length, 1);
  assert.equal(state.reviews[0].report.findings.length, 3);
  const items = conversationItems(state);
  const review = items.find(item => item.kind === 'review');
  assert.ok(review);
  const user = items.find(item => item.kind === 'user' && item.message.text.includes('/review'));
  assert.ok(review.seq > user.seq);
});

test('a live terminal survives the history reload that races it', () => {
  const restored = projectSnapshot({ id: 'session-1', messages: [], task_status: 'idle', pending_interactions: [], active_tools: [] }, [
    { event: { type: 'review_started', review: summary() } },
  ]);
  const live = applyEvent(restored, { type: 'review_finished', review: summary({ status: 'completed', report_source: 'submitted' }), report: report() });
  const merged = mergeRestoredConversation(restored, live);
  assert.equal(merged.reviews.length, 1);
  assert.equal(merged.reviews[0].summary.status, 'completed');
  assert.equal(merged.reviews[0].report.findings.length, 3);
});

test('scope text names the object the runtime will read', () => {
  assert.equal(reviewScopeText({ kind: 'commit', rev: 'abc1234' }), '提交 abc1234');
  assert.equal(reviewScopeText({ kind: 'branch', base: 'origin/main' }), '相对 origin/main 的分支变更');
  assert.equal(reviewScopeText({ kind: 'paths', paths: ['a', 'b'] }), '指定路径：a、b');
  assert.equal(reviewScopeText({ kind: 'current_task' }), '当前任务（需求、交付与证据）');
  assert.equal(reviewScopeText(null), '未知范围');
});
