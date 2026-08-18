// RuntimeBridge：UI 与 runtime 之间的控制面。
// 持有 WsClient，把下行帧翻译成 reducer action；向上给组件暴露用户操作
// （发消息、审批、切模型/权限/模式、斜杠命令、消息队列等）。

import type { Dispatch } from 'react';
import * as api from './api';
import { formatClock, modelRefString } from './format';
import { getToken } from './token';
import { commandProgressLabel, turnEndFromEvent, turnProgressLabel } from './turn';
import { deliverFrame, WsClient } from './ws';
import type { Action, AppState } from '../state/store';
import type {
  ApprovalDecision,
  CheckpointId,
  ClientCommand,
  DownFrame,
  ModelRef,
  PermissionProfile,
  RuntimeEvent,
  SessionId,
  UiSessionSnapshot,
} from '../types/protocol';

/** 产品轴合法值（wire 契约：SetProductAxes 的注释）。 */
export const WORK_PROFILES = ['economy', 'balanced', 'delivery'] as const;
export const COLLABORATIONS = ['chat', 'plan', 'goal'] as const;

type GetState = () => AppState;

export class RuntimeBridge {
  private readonly ws: WsClient;
  private readonly dispatch: Dispatch<Action>;
  private readonly getState: GetState;
  /** selectSession 后等待的目标会话 id（防止采纳别会话的广播整量） */
  private pendingSessionId: SessionId | null = null;
  /** `/clear` 已发出、等待宿主返回新会话。新会话 id 由宿主分配，事先不知道，
   *  所以只能标记"下一个 session_opened 就是它"，并在采纳时切换 WS 订阅。 */
  private awaitingNewSession = false;

  constructor(dispatch: Dispatch<Action>, getState: GetState) {
    this.dispatch = dispatch;
    this.getState = getState;
    this.ws = new WsClient(getToken(), {
      onFrame: (frame) => this.handleFrame(frame),
      onStatus: (status) => this.dispatch({ type: 'connection', status }),
    });
  }

  start(): void {
    this.ws.connect();
    this.requestSessionList();
    void this.refreshProjects();
  }

  dispose(): void {
    this.ws.dispose();
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
        this.dispatch({ type: 'project_status', path: frame.path, status: frame.status });
        // 后台发现（历史项目自动注册）带来的新项目不在已拉取的列表里：
        // 状态帧到达时补拉一次，让分组立即出现。
        if (!this.getState().projects.some((p) => p.path === frame.path)) {
          void this.refreshProjects();
        }
        return;
      case 'error':
        this.dispatch({ type: 'notice', message: `服务端错误 ${frame.code}: ${frame.message}` });
        return;
      default:
        return; // 未知帧：忽略不崩
    }
  }

  private applySnapshot(snap: UiSessionSnapshot, contextWindow?: number | null): void {
    const { current, draft } = this.getState();
    // 广播流里可能夹带别会话的 session_opened/updated：只接收当前会话的整量；
    // 例外一是 selectSession 后等待目标会话 snapshot 的窗口期；
    // 例外二是 `/clear`：宿主刚建的新会话 id 与当前不同，正是要切过去的那个。
    if (current && current.id !== snap.id) {
      // 只接受 `/clear` 真正在等的那一个：宿主刚建的会话必然是空的。
      // 若换成"下一个不同 id 就采纳"，一旦 new_session_for 失败，这个标志
      // 会一直举着，把之后任意一个无关快照当成自己的新会话切过去。
      if (!this.awaitingNewSession || snap.messages.length > 0) return;
      this.awaitingNewSession = false;
      this.dispatch({ type: 'select_session', id: snap.id });
      this.ws.setSession(snap.id);
    }
    if (!current && (draft || (this.pendingSessionId !== null && snap.id !== this.pendingSessionId))) {
      return;
    }
    this.pendingSessionId = null;
    this.dispatch({ type: 'snapshot', session: snap, contextWindow });
    // 整量落地后若回合空闲，补发排队消息
    this.flushQueue();
  }

  private applyEvent(ev: RuntimeEvent): void {
    const state = this.getState();
    switch (ev.type) {
      case 'session_list':
        this.dispatch({ type: 'session_list', sessions: ev.sessions });
        return;
      case 'session_opened':
      case 'session_updated':
        this.applySnapshot(ev.session);
        return;
      case 'runtime_ready':
        this.requestSessionList();
        return;
      default:
        break;
    }

    const current = state.current;
    if (!current) return; // 事件不带会话维度：无当前会话时无法落位，忽略

    switch (ev.type) {
      case 'user_message_added':
        this.dispatch({
          type: 'user_message',
          id: ev.message.id,
          text: ev.message.text,
          time: formatClock(),
        });
        break;
      case 'assistant_message_started':
        this.dispatch({ type: 'assistant_started', id: ev.message_id, time: formatClock() });
        break;
      case 'assistant_attempt_reset':
        this.dispatch({ type: 'assistant_reset', id: ev.message_id ?? null });
        break;
      case 'assistant_text_delta':
        this.dispatch({ type: 'assistant_delta', id: ev.message_id, delta: ev.delta });
        break;
      case 'assistant_message_completed':
        this.dispatch({ type: 'assistant_completed', id: ev.message_id });
        break;
      case 'tool_call_started':
        this.dispatch({
          type: 'tool_started',
          id: ev.id,
          name: ev.name,
          arguments: ev.arguments,
          parallel: ev.parallel ?? false,
        });
        break;
      case 'tool_call_completed':
        this.dispatch({
          type: 'tool_completed',
          id: ev.id,
          ok: ev.ok,
          preview: ev.preview,
          durationMs: ev.duration_ms,
        });
        break;
      case 'approval_requested':
        this.dispatch({ type: 'approval_requested', request: ev.request });
        break;
      case 'approval_resolved':
        this.dispatch({ type: 'approval_resolved', requestId: ev.id });
        break;
      case 'clarification_requested':
        this.dispatch({ type: 'clarification_requested', request: ev.request });
        break;
      case 'clarification_resolved':
        this.dispatch({ type: 'clarification_resolved', requestId: ev.id });
        break;
      case 'plan_updated':
        this.dispatch({ type: 'plan', plan: ev.plan });
        break;
      case 'verification_updated':
        this.dispatch({ type: 'verification', verification: ev.verification });
        break;
      case 'diff_updated':
        this.dispatch({ type: 'diff', diff: ev.diff });
        break;
      case 'checkpoint_created':
        this.dispatch({ type: 'checkpoint_added', checkpoint: ev.checkpoint });
        break;
      case 'session_completed':
        this.dispatch({ type: 'completion', report: ev.report });
        break;
      case 'token_usage':
        this.dispatch({
          type: 'token_usage',
          input: ev.input_tokens,
          output: ev.output_tokens,
        });
        break;
      case 'btw_started':
        this.dispatch({ type: 'btw_started', question: ev.question, time: formatClock() });
        break;
      case 'btw_text_delta':
        this.dispatch({ type: 'btw_delta', delta: ev.delta });
        break;
      case 'btw_completed':
        this.dispatch({ type: 'btw_done' });
        break;
      case 'btw_failed':
        this.dispatch({ type: 'btw_done' });
        this.dispatch({ type: 'notice', message: `btw 失败：${ev.error}` });
        break;
      case 'agent_activity':
        this.dispatch({ type: 'agent_activity', label: ev.label });
        break;
      case 'attachment_added':
        this.dispatch({ type: 'attachment_added', attachment: ev.attachment });
        break;
      case 'attachment_processing_failed':
        this.dispatch({ type: 'notice', message: `附件处理失败：${ev.error}` });
        break;
      case 'notification':
        // info 也展示：runtime 的结构化事实（bg 任务、memory 提示等）不许静默丢。
        this.dispatch({ type: 'notice', message: ev.message });
        break;
      case 'reasoning_delta':
        this.dispatch({ type: 'reasoning_delta', delta: ev.delta });
        break;
      case 'command_progress':
        this.dispatch({
          type: 'agent_activity',
          label: commandProgressLabel(ev.label, ev.elapsed_ms),
        });
        break;
      case 'turn_progress': {
        const label = turnProgressLabel(ev.phase, ev.closing, ev.no_progress_streak);
        if (label) this.dispatch({ type: 'agent_activity', label });
        break;
      }
      case 'sub_agent_updated':
        this.dispatch({
          type: 'sub_agent_updated',
          id: ev.id,
          nickname: ev.nickname,
          role: ev.role,
          done: ev.done,
          ok: ev.ok,
          detail: ev.detail,
        });
        break;
      case 'sub_agent_progress':
        this.dispatch({
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
        this.dispatch({ type: 'sub_agent_activity', id: ev.id, step });
        break;
      }
      case 'background_task_started':
        this.dispatch({
          type: 'background_started',
          taskId: ev.task_id,
          program: ev.program,
          args: ev.args,
        });
        break;
      case 'background_task_exited':
        this.dispatch({
          type: 'background_exited',
          taskId: ev.task_id,
          exitCode: ev.exit_code ?? null,
          durationMs: ev.duration_ms,
          ok: ev.ok,
        });
        break;
      case 'memory_list':
        this.dispatch({
          type: 'memory_list',
          dir: ev.memory_dir,
          active: ev.active,
          archived: ev.archived,
          pending: ev.pending ?? [],
        });
        break;
      case 'context_updated':
        this.dispatch({ type: 'context_estimate', tokens: ev.estimated_tokens });
        break;
      case 'context_compacted':
        this.dispatch({ type: 'notice', message: `上下文已压缩 ${ev.from} → ${ev.to} 条` });
        break;
      case 'context_expanded':
        this.dispatch({
          type: 'notice',
          message: `上下文预算已扩张 ${ev.from_tokens} → ${ev.to_tokens} tokens`,
        });
        break;
      default:
        // user_shell_*（web 无 !command 入口，本期不渲染）/ project_rules_loaded /
        // 未知（更新的 runtime 新增的）事件：忽略不崩。
        break;
    }

    const end = turnEndFromEvent(ev);
    if (end) {
      // 7 个终态逐一保真（Turn Truth）：incomplete/unverified 绝不折叠成 completed。
      this.dispatch({ type: 'turn_terminal', outcome: end.outcome, detail: end.detail });
      // dispatch 是异步的，getState() 还没落地，强制跳过 turnActive 检查
      this.flushQueue(true);
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
    this.dispatch({ type: 'select_session', id });
    this.ws.setSession(id);
    // 让 runtime 把该会话 transcript 载入视图（网关也会主动推 snapshot，双保险）
    this.deliver({ type: 'open_session', session_id: id });
  }

  newDraft(project?: string): void {
    this.dispatch({ type: 'new_draft', project: project ?? null });
  }

  // ── 多项目（聚合层）─────────────────────────────────────────────────

  async refreshProjects(): Promise<void> {
    try {
      const { projects } = await api.listProjects();
      this.dispatch({ type: 'projects', projects });
    } catch {
      // 单项目模式（无聚合层）或瞬时失败：静默，项目分组仍按会话列表渲染
    }
  }

  /** 把文本注入输入框（空状态快捷操作用）；Composer 消费后回传 null 清空。 */
  seedComposer(text: string | null): void {
    this.dispatch({ type: 'seed_composer', text });
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
          state.draftProject ?? undefined,
        );
        this.dispatch({
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
        this.dispatch({ type: 'turn_active', value: true });
        this.requestSessionList();
      } catch (err) {
        this.dispatch({
          type: 'notice',
          message: `创建会话失败：${err instanceof Error ? err.message : String(err)}`,
        });
      }
      return;
    }

    if (state.current.turnActive) {
      // 回合进行中：FIFO 排队，turn 终态后自动发下一条
      this.dispatch({
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
    if (state.pendingAttachments.length > 0) this.dispatch({ type: 'attachments_cleared' });
    this.dispatch({ type: 'turn_active', value: true });
  }

  /** turn 终态 / 重连恢复后：把当前会话队首消息发出去。force 用于刚 dispatch 完 turn_terminal 的窗口。 */
  flushQueue(force = false): void {
    const state = this.getState();
    if (!state.current || (!force && state.current.turnActive)) return;
    const next = state.queue.find((q) => q.sessionId === state.current?.id);
    if (!next) return;
    this.dispatch({ type: 'dequeue', id: next.id });
    this.deliver({
      type: 'submit_message',
      session_id: next.sessionId,
      content: next.text,
    });
    this.dispatch({ type: 'turn_active', value: true });
  }

  cancelQueued(id: string): void {
    this.dispatch({ type: 'dequeue', id });
  }

  /** 调整排队消息顺序（纯客户端队列，dir=-1 上移 / 1 下移）。 */
  moveQueued(id: string, dir: -1 | 1): void {
    this.dispatch({ type: 'queue_move', id, dir });
  }

  // ── 审批 / 澄清（固定 command_id，重试幂等）────────────────────────

  decideApproval(requestId: string, decision: ApprovalDecision): void {
    this.dispatch({ type: 'approval_resolved', requestId });
    this.deliver(
      { type: 'approval_decision', request_id: requestId, decision },
      `approval:${requestId}`,
    );
  }

  answerClarification(requestId: string, answer: string): void {
    this.dispatch({ type: 'clarification_resolved', requestId });
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

  setPermission(mode: PermissionProfile): void {
    const current = this.getState().current;
    this.dispatch({ type: 'set_permission', mode });
    if (current) this.deliver({ type: 'set_permission_profile', session_id: current.id, mode });
  }

  setModel(model: ModelRef): void {
    const current = this.getState().current;
    this.dispatch({ type: 'set_model', model });
    if (current) this.deliver({ type: 'select_model', session_id: current.id, model });
  }

  /** 设置产品轴（work_profile × collaboration）。SoT 在 session record；
   *  乐观更新本地视图，runtime 回 session_updated 确认。回合运行中不许切
   *  （与 TUI 的 idle-only 规则一致——runtime 只在空闲时接受）。 */
  setAxes(workProfile: string, collaboration: string): void {
    const current = this.getState().current;
    if (!current) return;
    if (current.turnActive) {
      this.notice('回合运行中不能切换运行轴，先停止或等它结束');
      return;
    }
    this.dispatch({ type: 'set_axes', workProfile, collaboration });
    this.deliver({
      type: 'set_product_axes',
      session_id: current.id,
      work_profile: workProfile,
      collaboration,
    });
    if (collaboration === 'plan') {
      this.notice('协作=计划（只读）。确认后切到 goal 开始执行');
    }
  }

  // ── 项目记忆（用户权威操作：接受/遗忘后刷新列表）───────────────────

  listMemory(): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'list_memory', session_id: current.id, include_archived: true });
  }

  acceptMemory(id: string): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'accept_memory', session_id: current.id, id });
    this.listMemory();
  }

  forgetMemory(id: string): void {
    const current = this.getState().current;
    if (!current) return;
    this.deliver({ type: 'forget_memory', session_id: current.id, id });
    this.listMemory();
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
    this.deliver({ type: 'request_diff', session_id: current.id });
  }

  /** 仅从待发列表移除附件（服务端已注册的无法撤回） */
  removeAttachment(id: string): void {
    this.dispatch({ type: 'attachment_removed', id });
  }

  dismissNotice(): void {
    this.dispatch({ type: 'notice', message: null });
  }

  notice(message: string): void {
    this.dispatch({ type: 'notice', message });
  }

  // ── 斜杠命令（mockup 里的 10 条）───────────────────────────────────

  runSlash(input: string): void {
    const [head, ...rest] = input.split(/\s+/);
    const arg = rest.join(' ').trim();
    const current = this.getState().current;
    const needSession = (): SessionId | null => {
      if (!current) {
        this.dispatch({ type: 'notice', message: '该命令需要先进入一个会话' });
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
          this.dispatch({ type: 'notice', message: `未知模型：${arg}` });
          return;
        }
        this.setModel(hit);
        return;
      }
      case '/work-mode': {
        if (!arg) return; // 无参数：由 Composer 打开工作档弹层
        const work = arg.toLowerCase();
        if (!(WORK_PROFILES as readonly string[]).includes(work)) {
          this.dispatch({ type: 'notice', message: '用法：/work-mode economy|balanced|delivery' });
          return;
        }
        const cur = this.getState().current;
        if (cur) this.setAxes(work, cur.collaboration);
        return;
      }
      case '/collab': {
        if (!arg) return; // 无参数：由 Composer 打开协作弹层
        const collab = arg.toLowerCase();
        if (!(COLLABORATIONS as readonly string[]).includes(collab)) {
          this.dispatch({ type: 'notice', message: '用法：/collab chat|plan|goal' });
          return;
        }
        const cur = this.getState().current;
        if (cur) this.setAxes(cur.workProfile, collab);
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
          this.dispatch({ type: 'notice', message: '用法：/perm ask|assist|full' });
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
        if (sid) this.deliver({ type: 'request_diff', session_id: sid });
        return;
      }
      case '/checkpoint': {
        const sid = needSession();
        if (!sid) return;
        if (!arg) {
          this.dispatch({ type: 'notice', message: '用法：/checkpoint <id>（CKPT 面板可点选）' });
          return;
        }
        this.deliver({ type: 'restore_checkpoint', session_id: sid, checkpoint_id: arg });
        return;
      }
      case '/memory': {
        const sid = needSession();
        if (!sid) return;
        this.listMemory();
        this.dispatch({ type: 'notice', message: '记忆已刷新，见右栏「记忆」' });
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
          this.dispatch({ type: 'notice', message: '用法：/btw <问题>' });
          return;
        }
        this.deliver({ type: 'btw', session_id: sid, question: arg });
        return;
      }
      default:
        this.dispatch({ type: 'notice', message: `未知命令：${head}（输入 / 查看命令面板）` });
    }
  }
}
