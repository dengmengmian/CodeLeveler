// RuntimeBridge：UI 与 runtime 之间的控制面。
// 持有 WsClient，把下行帧翻译成 reducer action；向上给组件暴露用户操作
// （发消息、审批、切模型/权限/模式、斜杠命令、消息队列等）。

import type { Dispatch } from 'react';
import * as api from './api';
import { formatClock, modelRefString } from './format';
import { loadLastSession, saveLastSession } from './lastSession';
import { getToken } from './token';
import { shouldRefreshObservability } from './observabilityView';
import { committedFinalAnswer } from './executionRounds';
import {
  commandProgressLabel,
  finalizationStageLabel,
  turnEndFromEvent,
  turnProgressLabel,
} from './turn';
import { deliverFrame, WsClient } from './ws';
import {
  initialState,
  reducer,
  type Action,
  type AppState,
  type SessionView,
} from '../state/store';
import type {
  ApprovalDecision,
  CheckpointId,
  ClientCommand,
  DownFrame,
  ModelRef,
  PermissionProfile,
  RuntimeEvent,
  SessionId,
  UiHistoryEntry,
  UiAgentDraft,
  UiAgentScope,
  UiMemoryKind,
  UiSessionSnapshot,
} from '../types/protocol';

/** 产品轴合法值（wire 契约：SetProductAxes 的注释）。 */
export const COLLABORATIONS = ['chat', 'plan', 'goal'] as const;

type GetState = () => AppState;

export class RuntimeBridge {
  private readonly ws: WsClient;
  private readonly dispatch: Dispatch<Action>;
  private readonly getState: GetState;
  /** Where actions currently go. A durable-history replay swaps this for a
   *  scratch reducer, so the ONE event mapping feeds both paths. */
  private sink: Dispatch<Action>;
  /** selectSession 后等待的目标会话 id（防止采纳别会话的广播整量） */
  private pendingSessionId: SessionId | null = null;
  /** Only this client's latest list-memory query may replace its Memory view. */
  private pendingMemoryQueryId: string | null = null;
  private pendingAgentListQueryId: string | null = null;
  private pendingAgentDetailQueryId: string | null = null;
  private readonly pendingAgentMutationQueryIds = new Set<string>();
  private pendingDiffQueryId: string | null = null;
  /** The client's own in-flight session-history query, so a foreign or stale
   *  answer never replaces this client's transcript. */
  private pendingHistoryQueryId: string | null = null;
  /** Sessions whose durable history this connection already loaded. */
  private readonly historyLoaded = new Set<SessionId>();
  /** True while durable history is being replayed into a scratch view. The
   *  event mapping then reads THAT view, not the live one: "does this turn have
   *  a committed answer?" must be asked of the transcript being rebuilt. */
  private replaying = false;
  private replayView: SessionView | null = null;
  /** `/clear` 已发出、等待宿主返回新会话。新会话 id 由宿主分配，事先不知道，
   *  所以只能标记"下一个 session_opened 就是它"，并在采纳时切换 WS 订阅。 */
  private awaitingNewSession = false;

  constructor(dispatch: Dispatch<Action>, getState: GetState) {
    this.dispatch = dispatch;
    this.sink = dispatch;
    this.getState = getState;
    this.ws = new WsClient(getToken(), {
      onFrame: (frame) => this.handleFrame(frame),
      onStatus: (status) => this.sink({ type: 'connection', status }),
    });
  }

  start(): void {
    this.ws.connect();
    this.requestSessionList();
    void this.refreshProjects();
  }

  dispose(): void {
    this.ws.dispose();
    this.sink = this.dispatch;
  }

  // ── 下行帧 ────────────────────────────────────────────────────────

  private handleFrame(frame: DownFrame): void {
    switch (frame.type) {
      case 'event':
        this.applyEvent(frame.event);
        return;
      case 'snapshot':
        this.applySnapshot(frame.session);
        return;
      case 'ack':
        return; // 送达回执，目前无需展示
      case 'project_status':
        this.sink({ type: 'project_status', path: frame.path, status: frame.status });
        // 后台发现（历史项目自动注册）带来的新项目不在已拉取的列表里：
        // 状态帧到达时补拉一次，让分组立即出现。
        if (!this.getState().projects.some((p) => p.path === frame.path)) {
          void this.refreshProjects();
        }
        return;
      case 'error':
        this.sink({ type: 'notice', message: `服务端错误 ${frame.code}: ${frame.message}` });
        return;
      default:
        return; // 未知帧：忽略不崩
    }
  }

  queryObservability(sessionId?: SessionId): void {
    const id = sessionId ?? this.getState().current?.id;
    if (!id) return;
    const queryId = crypto.randomUUID();
    this.sink({ type: 'observation_loading', queryId });
    this.deliver({
      type: 'query_observability',
      session_id: id,
      query_id: queryId,
      before: 0,
      after: 80,
    });
  }

  /**
   * Ask the runtime to project its own durable log into the same client events
   * a live session produced, and adopt it as the conversation.
   *
   * The snapshot's `messages` are the ACTIVE MODEL CONTEXT: `/compact` leaves a
   * single summary row there, so a reopened session would otherwise read as if
   * it started mid-task. The runtime already answers this command with
   * normalized `RuntimeEvent`s (user, reasoning, tool calls with their applied
   * diff, notices, terminals) — this client replays them through the SAME
   * reducer the live stream uses.
   */
  private requestSessionHistory(sessionId: SessionId): void {
    const current = this.getState().current;
    if (!current || current.id !== sessionId) return;
    // A running turn owns the transcript: replaying under it would fight the
    // live events for the same rows.
    if (current.turnActive) return;
    if (this.pendingHistoryQueryId !== null) return;
    const queryId = crypto.randomUUID();
    this.pendingHistoryQueryId = queryId;
    this.deliver({
      type: 'query_session_history',
      session_id: sessionId,
      query_id: queryId,
    });
  }

  /**
   * Replay durable history through the one event mapping.
   *
   * The mapping lives in `applyEvent`; swapping the action sink for a scratch
   * reducer reuses it instead of growing a second, drifting one. Only the
   * transcript fields are adopted back, so live state (turnActive, activity,
   * plan, diff, approvals, tokens) is never overwritten by the past.
   */
  private replayHistory(entries: UiHistoryEntry[], omittedTurns: number): void {
    const current = this.getState().current;
    if (!current || entries.length === 0) return;
    let scratch: AppState = structuredClone(initialState);
    scratch.draft = false;
    scratch.current = {
      ...structuredClone(current),
      messages: [],
      tools: [],
      thoughts: [],
      traces: [],
      lastTurn: null,
      historyOmittedTurns: omittedTurns,
      reasoning: '',
      reasoningSuperseded: false,
      turnActive: false,
      turnStartedAt: null,
      activity: null,
    };
    const live = this.sink;
    this.replaying = true;
    this.replayView = scratch.current;
    this.sink = (action: Action) => {
      // The reducer mutates in place, exactly as it does on the live path.
      reducer(scratch, action);
      this.replayView = scratch.current;
    };
    try {
      for (const entry of entries) this.applyEvent(entry.event);
    } finally {
      this.sink = live;
      this.replaying = false;
      this.replayView = null;
    }
    const rebuilt = scratch.current;
    if (!rebuilt) return;
    this.sink({
      type: 'history_replaced',
      view: {
        messages: rebuilt.messages,
        tools: rebuilt.tools,
        thoughts: rebuilt.thoughts,
        traces: rebuilt.traces,
        lastTurn: rebuilt.lastTurn,
      },
      omittedTurns,
    });
  }

  private applySnapshot(snap: UiSessionSnapshot, contextWindow?: number | null): void {
    const { current, draft } = this.getState();
    const previousId = current?.id;
    // A reopen (this client is being handed a session it was not showing) is
    // the other moment the durable log, not the model context, is the truth.
    const reopen = previousId !== undefined && previousId !== snap.id;
    // 广播流里可能夹带别会话的 session_opened：只接收当前会话的整量；
    // 例外一是 selectSession 后等待目标会话 snapshot 的窗口期；
    // 例外二是 `/clear`：宿主刚建的新会话 id 与当前不同，正是要切过去的那个。
    if (current && current.id !== snap.id) {
      // 只接受 `/clear` 真正在等的那一个：宿主刚建的会话必然是空的。
      // 若换成"下一个不同 id 就采纳"，一旦 new_session_for 失败，这个标志
      // 会一直举着，把之后任意一个无关快照当成自己的新会话切过去。
      if (!this.awaitingNewSession || snap.messages.length > 0) return;
      this.awaitingNewSession = false;
      this.sink({ type: 'select_session', id: snap.id });
      this.ws.setSession(snap.id);
    }
    if (!current && (draft || (this.pendingSessionId !== null && snap.id !== this.pendingSessionId))) {
      return;
    }
    this.pendingSessionId = null;
    this.sink({ type: 'snapshot', session: snap, contextWindow });
    saveLastSession(snap.id);
    if (reopen) {
      this.historyLoaded.delete(snap.id);
      this.requestSessionHistory(snap.id);
    }
    if (previousId !== snap.id || this.getState().observation === null) {
      this.queryObservability(snap.id);
    }
    // 整量落地后若回合空闲，补发排队消息
    this.flushQueue();
  }

  /** SessionUpdated = TUI apply_meta. Same-session header only; never a full resync. */
  private applySessionMeta(snap: UiSessionSnapshot): void {
    const { current } = this.getState();
    if (!current || current.id !== snap.id) return;
    this.sink({ type: 'session_meta', session: snap });
  }

  private applyEvent(ev: RuntimeEvent): void {
    const state = this.getState();
    switch (ev.type) {
      case 'session_list':
        this.sink({ type: 'session_list', sessions: ev.sessions });
        this.maybeRestoreLastSession();
        return;
      case 'session_opened':
        this.applySnapshot(ev.session);
        // The runtime pushed a snapshot, which means the transcript may have
        // changed under us (`/compact`, a restored checkpoint, a resumed
        // session). Re-read the durable log rather than trusting the model
        // context the snapshot carries.
        this.requestSessionHistory(ev.session.id);
        return;
      case 'session_updated':
        this.applySessionMeta(ev.session);
        return;
      case 'runtime_ready':
        this.requestSessionList();
        return;
      default:
        break;
    }

    // A replay feeds the scratch view; the live stream feeds the live one. Same
    // mapping, one source of truth about what is currently on screen.
    const current = this.replayView ?? state.current;
    if (!current) return; // 事件不带会话维度：无当前会话时无法落位，忽略

    switch (ev.type) {
      case 'user_message_added':
        this.sink({
          type: 'user_message',
          id: ev.message.id,
          text: ev.message.text,
          time: formatClock(),
        });
        break;
      case 'assistant_message_started':
        this.sink({ type: 'assistant_started', id: ev.message_id, time: formatClock() });
        break;
      case 'assistant_attempt_reset':
        this.sink({ type: 'assistant_reset', id: ev.message_id ?? null });
        break;
      case 'assistant_text_delta':
        this.sink({ type: 'assistant_delta', id: ev.message_id, delta: ev.delta });
        break;
      case 'assistant_message_completed':
        this.sink({ type: 'assistant_completed', id: ev.message_id });
        break;
      case 'tool_call_started':
        this.sink({
          type: 'tool_started',
          id: ev.id,
          name: ev.name,
          arguments: ev.arguments,
          parallel: ev.parallel ?? false,
          modelStep: ev.model_step ?? null,
          answerEffect: ev.answer_effect ?? 'work',
        });
        break;
      case 'tool_call_completed':
        this.sink({
          type: 'tool_completed',
          id: ev.id,
          ok: ev.ok,
          preview: ev.preview,
          durationMs: ev.duration_ms,
          stop: ev.stop ?? null,
          // The runtime's CONFIRMED diff. Presentation never rebuilds one from
          // the tool's arguments: the requested patch is not the applied result.
          appliedDiff: ev.applied_diff ?? null,
        });
        break;
      case 'approval_requested':
        this.sink({ type: 'approval_requested', request: ev.request });
        break;
      case 'approval_resolved':
        this.sink({ type: 'approval_resolved', requestId: ev.id });
        break;
      case 'clarification_requested':
        this.sink({ type: 'clarification_requested', request: ev.request });
        break;
      case 'clarification_resolved':
        this.sink({ type: 'clarification_resolved', requestId: ev.id });
        break;
      case 'plan_updated':
        this.sink({ type: 'plan', plan: ev.plan });
        break;
      case 'diff_updated':
        if (ev.query_id) {
          if (ev.query_id !== this.pendingDiffQueryId) break;
          this.pendingDiffQueryId = null;
        }
        this.sink({ type: 'diff', diff: ev.diff });
        break;
      case 'checkpoint_created':
        this.sink({ type: 'checkpoint_added', checkpoint: ev.checkpoint });
        break;
      case 'session_completed':
        this.sink({ type: 'completion', report: ev.report });
        break;
      case 'token_usage':
        this.sink({
          type: 'token_usage',
          input: ev.input_tokens,
          output: ev.output_tokens,
        });
        break;
      case 'btw_started':
        this.sink({ type: 'btw_started', question: ev.question, time: formatClock() });
        break;
      case 'btw_text_delta':
        this.sink({ type: 'btw_delta', delta: ev.delta });
        break;
      case 'btw_completed':
        this.sink({ type: 'btw_done' });
        break;
      case 'btw_failed':
        this.sink({ type: 'btw_done' });
        this.sink({ type: 'notice', message: `btw 失败：${ev.error}` });
        break;
      case 'agent_activity':
        this.sink({ type: 'agent_activity', label: ev.label });
        break;
      case 'attachment_added':
        this.sink({ type: 'attachment_added', attachment: ev.attachment });
        break;
      case 'attachment_processing_failed':
        this.sink({ type: 'notice', message: `附件处理失败：${ev.error}` });
        break;
      case 'notification':
        // info 也展示：runtime 的结构化事实（bg 任务、memory 提示等）不许静默丢。
        this.sink({ type: 'notice', message: ev.message });
        break;
      case 'reasoning_delta':
        this.sink({ type: 'reasoning_delta', delta: ev.delta });
        break;
      case 'session_history_loaded': {
        // Only this client's own answer, for the session on screen.
        if (this.pendingHistoryQueryId !== null && ev.query_id === this.pendingHistoryQueryId) {
          this.pendingHistoryQueryId = null;
        } else if (ev.query_id !== undefined && ev.query_id !== null) {
          break;
        }
        if (ev.session_id !== current.id) break;
        this.historyLoaded.add(ev.session_id);
        this.replayHistory(ev.entries ?? [], ev.omitted_turns ?? 0);
        break;
      }
      case 'reasoning_completed':
        this.sink({
          type: 'reasoning_completed',
          elapsedMs: ev.elapsed_ms,
          // The segment's text is what the live row accumulated; the runtime
          // measured the duration, the UI never invents one.
          text: current.reasoning,
        });
        break;
      case 'command_progress':
        this.sink({
          type: 'agent_activity',
          label: commandProgressLabel(ev.label, ev.elapsed_ms),
        });
        break;
      case 'turn_progress': {
        const label = turnProgressLabel(ev.phase, ev.closing, ev.no_progress_streak);
        if (label) this.sink({ type: 'agent_activity', label });
        break;
      }
      case 'turn_finalizing':
        this.sink({ type: 'agent_activity', label: finalizationStageLabel(ev.stage) });
        break;
      case 'sub_agent_updated':
        this.sink({
          type: 'sub_agent_updated',
          id: ev.id,
          nickname: ev.nickname,
          role: ev.role,
          done: ev.done,
          ok: ev.ok,
          detail: ev.detail,
          outcome: ev.outcome ?? null,
          stop: ev.stop ?? null,
          profileId: ev.profile_id ?? null,
          readOnly: ev.read_only ?? false,
          background: ev.background ?? false,
          scope: ev.scope ?? [],
          agent: ev.agent ?? null,
        });
        break;
      case 'sub_agent_state_changed':
        this.sink({ type: 'sub_agent_state_changed', id: ev.id, state: ev.state });
        break;
      case 'sub_agent_progress':
        this.sink({
          type: 'sub_agent_progress',
          id: ev.id,
          active: ev.active,
          input: ev.input_tokens,
          output: ev.output_tokens,
          cached: ev.cached_input_tokens,
        });
        break;
      case 'sub_agent_activity': {
        // TUI 同款：完成的一步带 ✓/✗，进行中的只显示工具名。
        const step =
          ev.phase === 'tool_finished' ? `${ev.tool} ${ev.is_error ? '✗' : '✓'}` : ev.tool;
        this.sink({ type: 'sub_agent_activity', id: ev.id, step });
        break;
      }
      case 'background_task_started':
        this.sink({
          type: 'background_started',
          taskId: ev.task_id,
          program: ev.program,
          args: ev.args,
        });
        break;
      case 'background_task_exited':
        this.sink({
          type: 'background_exited',
          taskId: ev.task_id,
          exitCode: ev.exit_code ?? null,
          durationMs: ev.duration_ms,
          ok: ev.ok,
        });
        break;
      case 'memory_list':
        if (!ev.query_id || ev.query_id !== this.pendingMemoryQueryId) break;
        this.pendingMemoryQueryId = null;
        this.sink({
          type: 'memory_list',
          dir: ev.memory_dir,
          active: ev.active,
          archived: ev.archived,
          pending: ev.pending ?? [],
        });
        break;
      case 'agents_loaded':
        if (!ev.query_id || ev.query_id !== this.pendingAgentListQueryId) break;
        this.pendingAgentListQueryId = null;
        this.sink({ type: 'agents_loaded', entries: ev.agents, problems: ev.problems ?? [] });
        break;
      case 'agent_loaded':
        if (!ev.query_id || ev.query_id !== this.pendingAgentDetailQueryId) break;
        this.pendingAgentDetailQueryId = null;
        this.sink({
          type: 'agent_loaded',
          name: ev.name,
          agent: ev.agent ?? null,
          error: ev.error ?? null,
        });
        break;
      case 'agent_mutated':
        if (!ev.query_id || !this.pendingAgentMutationQueryIds.delete(ev.query_id)) break;
        this.sink({
          type: 'agent_mutated',
          name: ev.name,
          ok: ev.ok,
          error: ev.error ?? null,
          queryId: ev.query_id ?? null,
        });
        // 写成功才有新列表可拉；失败时 runtime 什么也没写。
        if (ev.ok) this.listAgents();
        break;
      case 'context_updated':
        this.sink({ type: 'context_estimate', tokens: ev.estimated_tokens });
        break;
      case 'context_compacted':
        this.sink({ type: 'notice', message: `上下文已压缩 ${ev.from} → ${ev.to} 条` });
        break;
      case 'context_expanded':
        this.sink({
          type: 'notice',
          message: `上下文预算已扩张 ${ev.from_tokens} → ${ev.to_tokens} tokens`,
        });
        break;
      case 'observability_loaded':
        this.sink({
          type: 'observation_loaded',
          observation: ev.observation,
          queryId: ev.query_id ?? null,
        });
        break;
      default:
        // user_shell_*（web 无 !command 入口，本期不渲染）/ project_rules_loaded /
        // 未知（更新的 runtime 新增的）事件：忽略不崩。
        break;
    }

    // A replay is a paint of the past: it must not fire live queries or touch
    // the live queue. It still needs the turn terminal (that IS a transcript
    // fact), which is why this gate is per side effect and not an early return.
    if (
      !this.replaying &&
      ev.type !== 'observability_loaded' &&
      shouldRefreshObservability(ev)
    ) {
      this.queryObservability(current.id);
    }

    const end = turnEndFromEvent(ev);
    if (end) {
      // Turn Truth：7 个 runtime 终态逐一保真（incomplete/unverified 绝不折叠成
      // completed）。Completed/Answered 但本回合没有 commit 的 FinalAnswer 时，
      // 终态只能是 no_final_answer —— 工具跑完不等于任务做完（Contract v1 §I9）。
      const outcome =
        (end.outcome === 'completed' || end.outcome === 'answered') &&
        current &&
        committedFinalAnswer(current.messages, current.tools) === null
          ? 'no_final_answer'
          : end.outcome;
      this.sink({ type: 'turn_terminal', outcome, detail: end.detail });
      if (!this.replaying) {
        // dispatch 是异步的，getState() 还没落地，强制跳过 turnActive 检查
        this.flushQueue(true);
      }
    }
  }

  // ── 命令发送 ──────────────────────────────────────────────────────

  private deliver(command: ClientCommand, commandId?: string): void {
    const sessionId =
      command.type === 'request_session_list' || command.type === 'quit'
        ? (this.getState().current?.id ?? '')
        : ((command as { session_id?: SessionId }).session_id ??
          this.getState().current?.id ??
          '');
    this.ws.send(deliverFrame(sessionId, command, commandId));
  }

  requestSessionList(): void {
    this.deliver({ type: 'request_session_list' });
  }

  // ── 会话切换 / 新建 ───────────────────────────────────────────────

  selectSession(id: SessionId): void {
    // An explicit pick supersedes a pending `/clear`.
    this.awaitingNewSession = false;
    this.pendingSessionId = id;
    this.sink({ type: 'select_session', id });
    this.ws.setSession(id);
    saveLastSession(id);
    // 让 runtime 把该会话 transcript 载入视图（网关也会主动推 snapshot，双保险）
    this.deliver({ type: 'open_session', session_id: id });
  }

  newDraft(project?: string): void {
    const state = this.getState();
    const path = project ?? state.selectedProject ?? (state.repository || null);
    this.sink({ type: 'new_draft', project: path });
    saveLastSession(null);
    this.leaveSessionSubscription();
  }

  /** Refresh should reopen the conversation that was on screen, not the hero. */
  private maybeRestoreLastSession(): void {
    const state = this.getState();
    if (!state.draft || state.current || this.pendingSessionId) return;
    const last = loadLastSession();
    if (!last) return;
    if (!state.sessions.some((s) => s.id === last)) return;
    this.selectSession(last);
  }

  selectProject(path: string): void {
    const before = this.getState().current?.id ?? null;
    this.sink({ type: 'select_project', path });
    this.sink({ type: 'set_rail_nav', nav: 'sessions' });
    if (before && this.getState().current?.id !== before) {
      this.leaveSessionSubscription();
    }
  }

  /** Stop receiving the previous session's subscribe_session stream. */
  private leaveSessionSubscription(): void {
    this.pendingSessionId = null;
    this.awaitingNewSession = false;
    this.ws.setSession(null);
  }

  // ── 多项目（聚合层）─────────────────────────────────────────────────

  async refreshProjects(): Promise<void> {
    try {
      const { projects } = await api.listProjects();
      this.sink({ type: 'projects', projects });
    } catch {
      // 单项目模式（无聚合层）或瞬时失败：静默，项目分组仍按会话列表渲染
    }
  }

  /** 把文本注入输入框（空状态快捷操作用）；Composer 消费后回传 null 清空。 */
  seedComposer(text: string | null): void {
    this.sink({ type: 'seed_composer', text });
  }

  /** 重新运行 / 重试：取当前会话最后一条用户消息重发。 */
  rerunLast(): void {
    const current = this.getState().current;
    if (!current) return;
    const lastUser = [...current.messages].reverse().find((m) => m.role === 'user');
    if (lastUser) void this.sendUserMessage(lastUser.text);
  }

  async addProject(path: string): Promise<boolean> {
    try {
      await api.addProject(path);
      await this.refreshProjects();
      this.sink({ type: 'select_project', path });
      this.requestSessionList();
      return true;
    } catch (err) {
      this.notice(`打开项目失败：${err instanceof Error ? err.message : String(err)}`);
      return false;
    }
  }

  async removeProject(path: string): Promise<void> {
    try {
      await api.removeProject(path);
      await this.refreshProjects();
    } catch (err) {
      this.notice(`移除项目失败：${err instanceof Error ? err.message : String(err)}`);
    }
  }

  async restartProject(path: string): Promise<void> {
    try {
      await api.restartProject(path);
      this.notice('项目 daemon 重启中…');
    } catch (err) {
      this.notice(`重启失败：${err instanceof Error ? err.message : String(err)}`);
    }
  }

  async renameProject(path: string, name: string): Promise<void> {
    try {
      await api.renameProject(path, name);
      await this.refreshProjects();
    } catch (err) {
      this.notice(`重命名失败：${err instanceof Error ? err.message : String(err)}`);
    }
  }

  // ── 会话菜单（复制 ID / 重命名 / 分叉 / 导出 / 归档）────────────────

  renameSession(id: SessionId, name: string): void {
    const trimmed = name.trim();
    if (!trimmed) return;
    this.deliver({ type: 'rename_session', session_id: id, name: trimmed });
    // 同连接命令顺序处理:列表请求在改名落库后应答,兜底全局流丢帧
    this.requestSessionList();
  }

  archiveSession(id: SessionId): void {
    this.deliver({ type: 'archive_session', session_id: id });
    this.requestSessionList();
    this.notice('会话已归档');
  }

  deleteSession(id: SessionId): void {
    this.deliver({ type: 'delete_session', session_id: id });
    if (this.getState().current?.id === id) this.newDraft();
    this.requestSessionList();
  }

  forkSession(id: SessionId): void {
    this.deliver({ type: 'fork_session', session_id: id });
    this.requestSessionList();
  }

  async copySessionId(id: SessionId): Promise<void> {
    try {
      await navigator.clipboard.writeText(id);
      this.notice('已复制 Session ID');
    } catch {
      this.notice(`Session ID: ${id}`);
    }
  }

  /** 导出会话为 Markdown 下载（用 snapshot 数据,不需要新端点）。 */
  async exportSession(id: SessionId, title: string): Promise<void> {
    try {
      const snapshot = await api.fetchSnapshot(id);
      const lines: string[] = [`# ${title || id}`, ''];
      for (const m of snapshot.messages ?? []) {
        lines.push(m.role === 'user' ? '## 用户' : '## 助手', '', m.text, '');
      }
      const blob = new Blob([lines.join('\n')], { type: 'text/markdown' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `${(title || id).slice(0, 40).replace(/[\\/:*?"<>|]/g, '_')}.md`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      this.notice(`导出失败：${err instanceof Error ? err.message : String(err)}`);
    }
  }

  // ── 发消息（含排队）────────────────────────────────────────────────

  async sendUserMessage(raw: string): Promise<void> {
    const text = raw.trim();
    if (!text) return;
    const state = this.getState();

    if (text.startsWith('/')) {
      this.runSlash(text);
      return;
    }

    if (state.draft || !state.current) {
      // 空状态首条消息 = 新会话 goal：REST 建会话 → WS 订阅 → submit_message
      try {
        const bootstrap = await api.createSession(
          text,
          state.current?.model ?? null,
          state.current?.permission ?? 'assisted',
          state.draftProject ?? state.selectedProject ?? undefined,
        );
        this.sink({
          type: 'snapshot',
          session: bootstrap.session,
          contextWindow: bootstrap.context_window,
        });
        this.ws.setSession(bootstrap.session.id);
        this.deliver({
          type: 'submit_message',
          session_id: bootstrap.session.id,
          content: text,
        });
        // 乐观置位：在 user_message_added 回包之前就进入排队语义
        this.sink({ type: 'turn_active', value: true });
        this.requestSessionList();
      } catch (err) {
        this.sink({
          type: 'notice',
          message: `创建会话失败：${err instanceof Error ? err.message : String(err)}`,
        });
      }
      return;
    }

    if (state.current.turnActive) {
      // 回合进行中：FIFO 排队，turn 终态后自动发下一条
      this.sink({
        type: 'enqueue',
        item: { id: crypto.randomUUID(), sessionId: state.current.id, text },
      });
      return;
    }

    this.deliver({
      type: 'submit_message',
      session_id: state.current.id,
      content: text,
      ...(state.pendingAttachments.length > 0
        ? { attachments: state.pendingAttachments }
        : {}),
    });
    if (state.pendingAttachments.length > 0) this.sink({ type: 'attachments_cleared' });
    this.sink({ type: 'turn_active', value: true });
  }

  /** turn 终态 / 重连恢复后：把当前会话队首消息发出去。force 用于刚 dispatch 完 turn_terminal 的窗口。 */
  flushQueue(force = false): void {
    const state = this.getState();
    if (!state.current || (!force && state.current.turnActive)) return;
    const next = state.queue.find((q) => q.sessionId === state.current?.id);
    if (!next) return;
    this.sink({ type: 'dequeue', id: next.id });
    this.deliver({
      type: 'submit_message',
      session_id: next.sessionId,
      content: next.text,
    });
    this.sink({ type: 'turn_active', value: true });
  }

  cancelQueued(id: string): void {
    this.sink({ type: 'dequeue', id });
  }

  /** 调整排队消息顺序（纯客户端队列，dir=-1 上移 / 1 下移）。 */
  moveQueued(id: string, dir: -1 | 1): void {
    this.sink({ type: 'queue_move', id, dir });
  }

  // ── 审批 / 澄清（固定 command_id，重试幂等）────────────────────────

  decideApproval(requestId: string, decision: ApprovalDecision): void {
    this.sink({ type: 'approval_resolved', requestId });
    this.deliver(
      { type: 'approval_decision', request_id: requestId, decision },
      `approval:${requestId}`,
    );
  }

  answerClarification(requestId: string, answer: string): void {
    this.sink({ type: 'clarification_resolved', requestId });
    this.deliver(
      { type: 'answer_clarification', request_id: requestId, answer },
      `clarification:${requestId}`,
    );
  }

  // ── 输入舱控件 ────────────────────────────────────────────────────

  cancelTurn(): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'cancel_current_turn', session_id: current.id });
  }

  /** Stop one running child; its parent turn keeps running. */
  cancelChild(childId: string): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'cancel_child', session_id: current.id, child_id: childId });
  }

  setPermission(mode: PermissionProfile): void {
    const current = this.getState().current;
    this.sink({ type: 'set_permission', mode });
    if (current) this.deliver({ type: 'set_permission_profile', session_id: current.id, mode });
  }

  setModel(model: ModelRef): void {
    const current = this.getState().current;
    this.sink({ type: 'set_model', model });
    if (current) this.deliver({ type: 'select_model', session_id: current.id, model });
  }

  /** 设置协作方式。SoT 在 session record；
   *  乐观更新本地视图，runtime 回 session_updated 确认。回合运行中不许切
   *  （与 TUI 的 idle-only 规则一致——runtime 只在空闲时接受）。 */
  setCollaboration(collaboration: string): void {
    const current = this.getState().current;
    if (!current) return;
    if (current.turnActive) {
      this.notice('回合运行中不能切换运行轴，先停止或等它结束');
      return;
    }
    this.sink({ type: 'set_axes', collaboration });
    this.deliver({
      type: 'set_product_axes',
      session_id: current.id,
      work_profile: 'single', // Legacy wire field; ignored by current runtimes.
      collaboration,
    });
    if (collaboration === 'plan') {
      this.notice('协作=计划（只读）。确认后切到 goal 开始执行');
    }
  }

  // ── 项目记忆（用户权威操作：接受/遗忘后刷新列表）───────────────────

  listMemory(): string | null {
    const current = this.getState().current;
    if (!current) return null;
    const queryId = crypto.randomUUID();
    this.pendingMemoryQueryId = queryId;
    this.deliver({
      type: 'list_memory',
      session_id: current.id,
      query_id: queryId,
      include_archived: true,
    });
    return queryId;
  }

  acceptMemory(id: string): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'accept_memory', session_id: current.id, id });
    this.listMemory();
  }

  /** 归档一条已保存的记忆。只对 active 条目有效。 */
  forgetMemory(id: string): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'forget_memory', session_id: current.id, id });
    this.listMemory();
  }

  /** 拒绝一条待确认候选，并压制同一信号再次提议。
   *
   * 不能用 forgetMemory 代替：forget 只处理 active/archive，把 pending 的 id
   * 发给它什么也不会发生 —— 这正是 pending 行「忽略」按钮原来的 bug。 */
  rejectMemory(id: string): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'reject_memory', session_id: current.id, id });
    this.listMemory();
  }

  /** 用户自己直接写入一条记忆：不经过模型，也不产生待确认候选。 */
  rememberMemory(body: string, kind: UiMemoryKind): void {
    const current = this.getState().current;
    if (!current) return;
    const text = body.trim();
    if (!text) return;
    this.deliver({ type: 'remember_memory', session_id: current.id, body: text, kind });
    this.listMemory();
  }

  // ── Agents 注册表（用户命令即授权；runtime 校验并原子写入）──────────
  // 每个方法返回 query_id，让发起的表单认领自己的应答；无会话时返回 null。

  listAgents(): string | null {
    const current = this.getState().current;
    if (!current) return null;
    const queryId = crypto.randomUUID();
    this.pendingAgentListQueryId = queryId;
    this.sink({ type: 'agents_loading' });
    this.deliver({ type: 'list_agents', session_id: current.id, query_id: queryId });
    return queryId;
  }

  getAgent(name: string): string | null {
    const current = this.getState().current;
    if (!current) return null;
    const queryId = crypto.randomUUID();
    this.pendingAgentDetailQueryId = queryId;
    this.deliver({ type: 'get_agent', session_id: current.id, name, query_id: queryId });
    return queryId;
  }

  createAgent(scope: UiAgentScope, draft: UiAgentDraft): string | null {
    const current = this.getState().current;
    if (!current) return null;
    const queryId = crypto.randomUUID();
    this.pendingAgentMutationQueryIds.add(queryId);
    this.deliver({ type: 'create_agent', session_id: current.id, scope, draft, query_id: queryId });
    return queryId;
  }

  updateAgent(scope: UiAgentScope, draft: UiAgentDraft): string | null {
    const current = this.getState().current;
    if (!current) return null;
    const queryId = crypto.randomUUID();
    this.pendingAgentMutationQueryIds.add(queryId);
    this.deliver({ type: 'update_agent', session_id: current.id, scope, draft, query_id: queryId });
    return queryId;
  }

  deleteAgent(scope: UiAgentScope, name: string): string | null {
    const current = this.getState().current;
    if (!current) return null;
    const queryId = crypto.randomUUID();
    this.pendingAgentMutationQueryIds.add(queryId);
    this.deliver({ type: 'delete_agent', session_id: current.id, scope, name, query_id: queryId });
    return queryId;
  }

  restoreCheckpoint(checkpointId: CheckpointId): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'restore_checkpoint', session_id: current.id, checkpoint_id: checkpointId });
  }

  /** 主动拉一次工作区 diff（Diff 视图打开时刷新用） */
  requestDiff(): void {
    const current = this.getState().current;
    if (!current) return;
    const queryId = crypto.randomUUID();
    this.pendingDiffQueryId = queryId;
    this.deliver({ type: 'request_diff', session_id: current.id, query_id: queryId });
  }

  sendBtw(question: string): void {
    const q = question.trim();
    if (!q) return;
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'btw', session_id: current.id, question: q });
  }

  openChanges(): void {
    this.requestDiff();
    this.sink({ type: 'stage_view', view: 'diff' });
  }

  openMemory(): void {
    this.listMemory();
    this.sink({ type: 'set_inspector_more', open: true });
  }

  /** 仅从待发列表移除附件（服务端已注册的无法撤回） */
  removeAttachment(id: string): void {
    this.sink({ type: 'attachment_removed', id });
  }

  dismissNotice(): void {
    this.sink({ type: 'notice', message: null });
  }

  notice(message: string): void {
    this.sink({ type: 'notice', message });
  }

  // ── 斜杠命令（mockup 里的 10 条）───────────────────────────────────

  runSlash(input: string): void {
    const [head, ...rest] = input.split(/\s+/);
    const arg = rest.join(' ').trim();
    const current = this.getState().current;
    const needSession = (): SessionId | null => {
      if (!current) {
        this.sink({ type: 'notice', message: '该命令需要先进入一个会话' });
        return null;
      }
      return current.id;
    };

    switch (head) {
      case '/model': {
        if (!arg) return; // 无参数：由 Composer 打开模型弹层
        const sid = needSession();
        if (!sid) return;
        const hit = this.getState().current?.availableModels.find(
          (m) => m.model === arg || modelRefString(m) === arg,
        );
        if (!hit) {
          this.sink({ type: 'notice', message: `未知模型：${arg}` });
          return;
        }
        this.setModel(hit);
        return;
      }
      case '/collab': {
        if (!arg) return; // 无参数：由 Composer 打开协作弹层
        const collab = arg.toLowerCase();
        if (!(COLLABORATIONS as readonly string[]).includes(collab)) {
          this.sink({ type: 'notice', message: '用法：/collab chat|plan|goal' });
          return;
        }
        const cur = this.getState().current;
        if (cur) this.setCollaboration(collab);
        return;
      }
      case '/perm': {
        if (!arg) return;
        const map: Record<string, PermissionProfile> = {
          ask: 'request_approval',
          assist: 'assisted',
          assisted: 'assisted',
          full: 'full_access',
        };
        const profile = map[arg.toLowerCase()];
        if (!profile) {
          this.sink({ type: 'notice', message: '用法：/perm ask|assist|full' });
          return;
        }
        this.setPermission(profile);
        return;
      }
      case '/compact': {
        const sid = needSession();
        if (sid) this.deliver({ type: 'compact_context', session_id: sid });
        return;
      }
      case '/clear': {
        // Start a NEW session; the current one stays in the session list.
        // This used to send clear_conversation, which wiped the session in
        // place — the label said one thing and the wire said another.
        const sid = needSession();
        if (sid) {
          // The host assigns the id, so accept the next snapshot even though
          // it will not match the current session — otherwise applySnapshot
          // drops it and the view never switches.
          this.awaitingNewSession = true;
          this.deliver({ type: 'new_session_for', requester_session_id: sid });
        }
        return;
      }
      case '/diff': {
        const sid = needSession();
        if (sid) this.requestDiff();
        return;
      }
      case '/checkpoint': {
        const sid = needSession();
        if (!sid) return;
        if (!arg) {
          this.sink({ type: 'notice', message: '用法：/checkpoint <id>（CKPT 面板可点选）' });
          return;
        }
        this.deliver({ type: 'restore_checkpoint', session_id: sid, checkpoint_id: arg });
        return;
      }
      case '/memory': {
        const sid = needSession();
        if (!sid) return;
        this.listMemory();
        this.sink({ type: 'notice', message: '记忆已刷新，见右栏「记忆」' });
        return;
      }
      case '/cancel': {
        this.cancelTurn();
        return;
      }
      case '/btw': {
        const sid = needSession();
        if (!sid) return;
        if (!arg) {
          this.sink({ type: 'notice', message: '用法：/btw <问题>' });
          return;
        }
        this.deliver({ type: 'btw', session_id: sid, question: arg });
        return;
      }
      default:
        this.sink({ type: 'notice', message: `未知命令：${head}（输入 / 查看命令面板）` });
    }
  }
}
