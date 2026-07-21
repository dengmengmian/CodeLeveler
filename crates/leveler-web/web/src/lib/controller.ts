// RuntimeBridge：UI 与 runtime 之间的控制面。
// 持有 WsClient，把下行帧翻译成 reducer action；向上给组件暴露用户操作
// （发消息、审批、切模型/权限/模式、斜杠命令、消息队列等）。

import type { Dispatch } from 'react';
import * as api from './api';
import { formatClock, modelRefString } from './format';
import { getToken } from './token';
import { deliverFrame, WsClient } from './ws';
import type { Action, AgentMode, AppState } from '../state/store';
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
import { TURN_TERMINAL_TYPES } from '../types/protocol';

type GetState = () => AppState;

export class RuntimeBridge {
  private readonly ws: WsClient;
  private readonly dispatch: Dispatch<Action>;
  private readonly getState: GetState;
  /** selectSession 后等待的目标会话 id（防止采纳别会话的广播整量） */
  private pendingSessionId: SessionId | null = null;

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
    // 例外是 selectSession 后等待目标会话 snapshot 的窗口期
    if (current && current.id !== snap.id) return;
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
        this.dispatch({ type: 'assistant_reset', id: ev.message_id });
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
      case 'clarification_requested':
        this.dispatch({ type: 'clarification_requested', request: ev.request });
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
        if (ev.level !== 'info') this.dispatch({ type: 'notice', message: ev.message });
        break;
      default:
        // reasoning_delta / sub_agent_* / background_task_* /
        // memory_list / context_updated / turn_progress / 未知事件：忽略
        break;
    }

    if (TURN_TERMINAL_TYPES.has(ev.type)) {
      this.dispatch({ type: 'turn_terminal' });
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

  setAgentMode(mode: AgentMode): void {
    const current = this.getState().current;
    this.dispatch({ type: 'set_agent_mode', mode });
    if (current) {
      this.deliver({
        type: 'set_agent_mode',
        session_id: current.id,
        orchestrate: mode === 'plan',
      });
    }
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
      case '/mode': {
        if (!arg) return;
        const mode: AgentMode | null =
          arg.toLowerCase() === 'plan' ? 'plan' : arg.toLowerCase() === 'direct' ? 'direct' : null;
        if (!mode) {
          this.dispatch({ type: 'notice', message: '用法：/mode direct|plan' });
          return;
        }
        this.setAgentMode(mode);
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
        const sid = needSession();
        if (sid) this.deliver({ type: 'clear_conversation', session_id: sid });
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
        if (sid) this.deliver({ type: 'list_memory', session_id: sid, include_archived: false });
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
