// 应用状态：useReducer 单向数据流。
// 视图模型对齐 mockup 的三栏结构；数据源全部来自协议契约
// （UiSessionSnapshot 整量 + RuntimeEvent 增量）。

import { createContext, useContext, type Dispatch, type ReactNode } from 'react';
import { useImmerReducer } from '../lib/useImmerReducer';
import type {
  AttachmentRef,
  ModelRef,
  PermissionProfile,
  ProjectInfo,
  ProjectStatus,
  SessionId,
  ToolCallId,
  UiApprovalRequest,
  UiCheckpoint,
  UiClarificationRequest,
  UiCompletionReport,
  UiDiff,
  UiPlan,
  UiRole,
  UiSessionSnapshot,
  UiSessionSummary,
  UiVerification,
} from '../types/protocol';

// ── 视图模型 ────────────────────────────────────────────────────────

/** Monotonic stamp so the timeline can interleave messages and tool calls by
 *  when they actually happened, instead of piling every tool at the very end. */
let seqCounter = 0;
const nextSeq = (): number => (seqCounter += 1);

export interface ChatMessage {
  id: string;
  role: UiRole;
  text: string;
  /** true = 仍在接收 text_delta */
  streaming: boolean;
  /** 实时追加的消息有到达时间；snapshot 回放的消息没有时间 */
  time: string | null;
  /** 时间线排序戳（越小越早） */
  seq: number;
}

export interface ToolCallView {
  id: ToolCallId;
  name: string;
  arguments: string;
  status: 'run' | 'done' | 'fail';
  preview: string | null;
  durationMs: number | null;
  parallel: boolean;
  /** 时间线排序戳（越小越早） */
  seq: number;
}

export interface QueuedMessage {
  id: string;
  sessionId: SessionId;
  text: string;
}

export type AgentMode = 'direct' | 'plan';

/** 上一回合终态：用于在对话流内显示「已完成 / 执行失败 / 已停止 + 用时」。 */
export interface LastTurn {
  outcome: 'completed' | 'failed' | 'cancelled';
  ms: number;
  error: string | null;
}

export interface SessionView {
  id: SessionId;
  title: string;
  repository: string;
  branch: string | null;
  status: string;
  messages: ChatMessage[];
  /** 当前回合的工具调用（下一回合开始时清空） */
  tools: ToolCallView[];
  pendingApprovals: UiApprovalRequest[];
  pendingClarifications: UiClarificationRequest[];
  plan: UiPlan | null;
  verification: UiVerification | null;
  diff: UiDiff | null;
  checkpoints: UiCheckpoint[];
  completionReport: UiCompletionReport | null;
  turnActive: boolean;
  /** agent_activity 事件的最近一条标签（「正在分析项目结构」之类） */
  activity: string | null;
  /** 当前回合开始时间（epoch ms）；空闲时为 null，用于运行计时 */
  turnStartedAt: number | null;
  /** 上一回合终态（完成/失败/取消）；新回合开始时清空 */
  lastTurn: LastTurn | null;
  model: ModelRef | null;
  availableModels: ModelRef[];
  permission: PermissionProfile;
  /** DIRECT/PLAN 是客户端侧轴，映射到 set_agent_mode.orchestrate */
  agentMode: AgentMode;
  tokens: { input: number; output: number };
  /** 来自 SessionBootstrap；不知道时 CTX 表按 0% */
  contextWindow: number | null;
}

export type ConnectionStatus = 'connecting' | 'online';

export interface AppState {
  connection: ConnectionStatus;
  sessions: UiSessionSummary[];
  current: SessionView | null;
  /** true = 空状态 hero（新对话，尚未建会话） */
  draft: boolean;
  /** 当前 runtime 的仓库路径（分组回退值 + hero 项目选择器） */
  repository: string;
  queue: QueuedMessage[];
  notice: string | null;
  /** 已上传、待随下一条消息提交的附件 */
  pendingAttachments: AttachmentRef[];
  /** 聚合层注册的项目（含状态）；空数组表示尚未拉取或单项目模式 */
  projects: ProjectInfo[];
  /** 新对话的目标项目（= 项目分组上的 ＋ 入口）；null = 当前仓库 */
  draftProject: string | null;
  /** 待注入到输入框的文本（空状态快捷操作 → Composer 消费后清空） */
  composerSeed: string | null;
}

export const initialState: AppState = {
  connection: 'connecting',
  sessions: [],
  current: null,
  draft: true,
  repository: '',
  queue: [],
  notice: null,
  pendingAttachments: [],
  projects: [],
  draftProject: null,
  composerSeed: null,
};

// ── Actions ─────────────────────────────────────────────────────────

export type Action =
  | { type: 'connection'; status: ConnectionStatus }
  | { type: 'session_list'; sessions: UiSessionSummary[] }
  | { type: 'snapshot'; session: UiSessionSnapshot; contextWindow?: number | null }
  | { type: 'select_session'; id: SessionId }
  | { type: 'new_draft'; project?: string | null }
  | { type: 'user_message'; id: string; text: string; time: string }
  | { type: 'assistant_started'; id: string; time: string }
  | { type: 'assistant_reset'; id: string | null }
  | { type: 'assistant_delta'; id: string; delta: string }
  | { type: 'assistant_completed'; id: string }
  | { type: 'btw_started'; question: string; time: string }
  | { type: 'btw_delta'; delta: string }
  | { type: 'btw_done' }
  | { type: 'tool_started'; id: ToolCallId; name: string; arguments: string; parallel: boolean }
  | { type: 'tool_completed'; id: ToolCallId; ok: boolean; preview: string; durationMs: number }
  | { type: 'approval_requested'; request: UiApprovalRequest }
  | { type: 'approval_resolved'; requestId: string }
  | { type: 'clarification_requested'; request: UiClarificationRequest }
  | { type: 'clarification_resolved'; requestId: string }
  | { type: 'plan'; plan: UiPlan }
  | { type: 'verification'; verification: UiVerification }
  | { type: 'diff'; diff: UiDiff }
  | { type: 'checkpoint_added'; checkpoint: UiCheckpoint }
  | { type: 'completion'; report: UiCompletionReport }
  | { type: 'token_usage'; input: number; output: number }
  | { type: 'turn_active'; value: boolean }
  | { type: 'turn_terminal'; outcome: 'completed' | 'failed' | 'cancelled'; error?: string }
  | { type: 'seed_composer'; text: string | null }
  | { type: 'enqueue'; item: QueuedMessage }
  | { type: 'dequeue'; id: string }
  | { type: 'queue_move'; id: string; dir: -1 | 1 }
  | { type: 'set_permission'; mode: PermissionProfile }
  | { type: 'set_model'; model: ModelRef }
  | { type: 'set_agent_mode'; mode: AgentMode }
  | { type: 'agent_activity'; label: string }
  | { type: 'attachment_added'; attachment: AttachmentRef }
  | { type: 'attachment_removed'; id: string }
  | { type: 'attachments_cleared' }
  | { type: 'projects'; projects: ProjectInfo[] }
  | { type: 'project_status'; path: string; status: ProjectStatus }
  | { type: 'notice'; message: string | null };

// ── snapshot → SessionView ──────────────────────────────────────────

function viewFromSnapshot(
  snap: UiSessionSnapshot,
  prev: SessionView | null,
  contextWindow?: number | null,
): SessionView {
  const pendingApprovals: UiApprovalRequest[] = [];
  const pendingClarifications: UiClarificationRequest[] = [];
  for (const pi of snap.pending_interactions ?? []) {
    if (pi.type === 'approval') pendingApprovals.push(pi.request);
    else pendingClarifications.push(pi.request);
  }
  // On a snapshot the interleave order between history messages and in-flight
  // tools is not recoverable, so keep messages first, then the active tools.
  const messages: ChatMessage[] = snap.messages.map((m) => ({
    id: m.id,
    role: m.role,
    text: m.text,
    streaming: false,
    time: null,
    seq: nextSeq(),
  }));
  const tools: ToolCallView[] = (snap.active_tools ?? []).map((t) => ({
    id: t.id,
    name: t.name,
    arguments: t.arguments,
    status: 'run',
    preview: null,
    durationMs: null,
    parallel: false,
    seq: nextSeq(),
  }));
  const s = snap.status.toLowerCase();
  const turnActive =
    s.includes('run') ||
    s.includes('busy') ||
    tools.length > 0 ||
    pendingApprovals.length > 0 ||
    pendingClarifications.length > 0;
  const sameSession = prev !== null && prev.id === snap.id;
  return {
    id: snap.id,
    title: snap.goal || '未命名会话',
    repository: snap.repository,
    branch: snap.branch,
    status: snap.status,
    messages,
    tools,
    pendingApprovals,
    pendingClarifications,
    plan: snap.plan ?? null,
    verification: snap.verification ?? null,
    diff: snap.diff ?? null,
    checkpoints: snap.checkpoints ?? [],
    completionReport: snap.completion_report ?? null,
    turnActive,
    activity: sameSession ? prev.activity : null,
    turnStartedAt: sameSession ? prev.turnStartedAt : turnActive ? Date.now() : null,
    lastTurn: sameSession ? prev.lastTurn : null,
    model: snap.model,
    availableModels: snap.available_models ?? [],
    permission: snap.mode,
    agentMode: sameSession ? prev.agentMode : 'direct',
    tokens: sameSession ? prev.tokens : { input: 0, output: 0 },
    contextWindow: contextWindow ?? (sameSession ? prev.contextWindow : null),
  };
}

// ── reducer ─────────────────────────────────────────────────────────

export function reducer(state: AppState, action: Action): void {
  switch (action.type) {
    case 'connection':
      state.connection = action.status;
      return;
    case 'session_list':
      state.sessions = action.sessions;
      // 顺手更新当前会话在轨道的标题（goal 以后端为准）
      if (state.current) {
        const row = action.sessions.find((s) => s.id === state.current?.id);
        if (row?.goal) state.current.title = row.goal;
      }
      return;
    case 'snapshot': {
      // 整量重同步：以 snapshot 为准重建当前会话视图
      const view = viewFromSnapshot(action.session, state.current, action.contextWindow);
      state.current = view;
      state.draft = false;
      if (view.repository) state.repository = view.repository;
      return;
    }
    case 'select_session':
      state.draft = false;
      if (state.current?.id !== action.id) state.current = null; // 等 snapshot
      return;
    case 'new_draft':
      state.draft = true;
      state.current = null;
      state.draftProject = action.project ?? null;
      return;
    case 'user_message': {
      if (!state.current) return;
      if (state.current.messages.some((m) => m.id === action.id)) return;
      state.current.messages.push({
        id: action.id,
        role: 'user',
        text: action.text,
        streaming: false,
        time: action.time,
        seq: nextSeq(),
      });
      // 新回合开始：清掉上一回合的工具轨与终态
      state.current.tools = [];
      state.current.turnActive = true;
      state.current.turnStartedAt = Date.now();
      state.current.activity = null;
      state.current.lastTurn = null;
      return;
    }
    case 'assistant_started': {
      if (!state.current) return;
      if (state.current.messages.some((m) => m.id === action.id)) return;
      state.current.messages.push({
        id: action.id,
        role: 'assistant',
        text: '',
        streaming: true,
        time: action.time,
        seq: nextSeq(),
      });
      state.current.turnActive = true;
      return;
    }
    case 'assistant_reset': {
      if (!state.current) return;
      if (action.id === null) {
        // 清掉最后一条仍在流式输出的消息
        for (let i = state.current.messages.length - 1; i >= 0; i -= 1) {
          if (state.current.messages[i].streaming) {
            state.current.messages.splice(i, 1);
            break;
          }
        }
      } else {
        const msg = state.current.messages.find((m) => m.id === action.id);
        if (msg) msg.text = '';
      }
      return;
    }
    case 'assistant_delta': {
      const msg = state.current?.messages.find((m) => m.id === action.id);
      if (msg) msg.text += action.delta;
      return;
    }
    case 'assistant_completed': {
      const msg = state.current?.messages.find((m) => m.id === action.id);
      if (msg) msg.streaming = false;
      return;
    }
    case 'btw_started': {
      if (!state.current) return;
      state.current.messages.push({
        id: 'btw-live',
        role: 'assistant',
        text: '',
        streaming: true,
        time: action.time,
        seq: nextSeq(),
      });
      return;
    }
    case 'btw_delta': {
      const msg = state.current?.messages.find((m) => m.id === 'btw-live');
      if (msg) msg.text += action.delta;
      return;
    }
    case 'btw_done': {
      const msg = state.current?.messages.find((m) => m.id === 'btw-live');
      if (msg) {
        msg.streaming = false;
        msg.id = `btw-${Date.now()}`;
      }
      return;
    }
    case 'tool_started': {
      if (!state.current) return;
      if (state.current.tools.some((t) => t.id === action.id)) return;
      state.current.tools.push({
        id: action.id,
        name: action.name,
        arguments: action.arguments,
        status: 'run',
        preview: null,
        durationMs: null,
        parallel: action.parallel,
        seq: nextSeq(),
      });
      return;
    }
    case 'tool_completed': {
      const tool = state.current?.tools.find((t) => t.id === action.id);
      if (tool) {
        tool.status = action.ok ? 'done' : 'fail';
        tool.preview = action.preview || null;
        tool.durationMs = action.durationMs;
      }
      return;
    }
    case 'approval_requested':
      if (state.current && !state.current.pendingApprovals.some((a) => a.id === action.request.id)) {
        state.current.pendingApprovals.push(action.request);
      }
      return;
    case 'approval_resolved':
      if (state.current) {
        state.current.pendingApprovals = state.current.pendingApprovals.filter(
          (a) => a.id !== action.requestId,
        );
      }
      return;
    case 'clarification_requested':
      if (
        state.current &&
        !state.current.pendingClarifications.some((c) => c.id === action.request.id)
      ) {
        state.current.pendingClarifications.push(action.request);
      }
      return;
    case 'clarification_resolved':
      if (state.current) {
        state.current.pendingClarifications = state.current.pendingClarifications.filter(
          (c) => c.id !== action.requestId,
        );
      }
      return;
    case 'plan':
      if (state.current) state.current.plan = action.plan;
      return;
    case 'verification':
      if (state.current) state.current.verification = action.verification;
      return;
    case 'diff':
      if (state.current) state.current.diff = action.diff;
      return;
    case 'checkpoint_added':
      if (state.current && !state.current.checkpoints.some((c) => c.id === action.checkpoint.id)) {
        state.current.checkpoints.push(action.checkpoint);
      }
      return;
    case 'completion':
      if (state.current) state.current.completionReport = action.report;
      return;
    case 'token_usage':
      if (state.current) {
        state.current.tokens = { input: action.input, output: action.output };
      }
      return;
    case 'turn_active':
      if (state.current) {
        if (action.value && !state.current.turnActive) {
          state.current.turnStartedAt = Date.now();
          state.current.activity = null;
          state.current.lastTurn = null;
        }
        if (!action.value) state.current.turnStartedAt = null;
        state.current.turnActive = action.value;
      }
      return;
    case 'turn_terminal':
      if (state.current) {
        const ms = state.current.turnStartedAt
          ? Math.max(0, Date.now() - state.current.turnStartedAt)
          : 0;
        state.current.lastTurn = {
          outcome: action.outcome,
          ms,
          error: action.error ?? null,
        };
        state.current.turnActive = false;
        state.current.turnStartedAt = null;
        state.current.activity = null;
        for (const m of state.current.messages) m.streaming = false;
      }
      return;
    case 'seed_composer':
      state.composerSeed = action.text;
      return;
    case 'enqueue':
      state.queue.push(action.item);
      return;
    case 'dequeue':
      state.queue = state.queue.filter((q) => q.id !== action.id);
      return;
    case 'queue_move': {
      const i = state.queue.findIndex((q) => q.id === action.id);
      const j = i + action.dir;
      if (i < 0 || j < 0 || j >= state.queue.length) return;
      const [item] = state.queue.splice(i, 1);
      state.queue.splice(j, 0, item);
      return;
    }
    case 'set_permission':
      if (state.current) state.current.permission = action.mode;
      return;
    case 'set_model':
      if (state.current) state.current.model = action.model;
      return;
    case 'set_agent_mode':
      if (state.current) state.current.agentMode = action.mode;
      return;
    case 'agent_activity':
      if (state.current) state.current.activity = action.label;
      return;
    case 'attachment_added':
      if (!state.pendingAttachments.some((a) => a.id === action.attachment.id)) {
        state.pendingAttachments.push(action.attachment);
      }
      return;
    case 'attachment_removed':
      state.pendingAttachments = state.pendingAttachments.filter((a) => a.id !== action.id);
      return;
    case 'attachments_cleared':
      state.pendingAttachments = [];
      return;
    case 'projects':
      state.projects = action.projects;
      return;
    case 'project_status': {
      const p = state.projects.find((p) => p.path === action.path);
      if (p) p.status = action.status;
      return;
    }
    case 'notice':
      state.notice = action.message;
      return;
  }
}

// ── React 绑定 ──────────────────────────────────────────────────────

const StateContext = createContext<AppState | null>(null);
const DispatchContext = createContext<Dispatch<Action> | null>(null);

export function AppProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useImmerReducer(reducer, initialState);
  return (
    <StateContext.Provider value={state}>
      <DispatchContext.Provider value={dispatch}>{children}</DispatchContext.Provider>
    </StateContext.Provider>
  );
}

export function useAppState(): AppState {
  const state = useContext(StateContext);
  if (!state) throw new Error('useAppState 必须在 AppProvider 内使用');
  return state;
}

export function useAppDispatch(): Dispatch<Action> {
  const dispatch = useContext(DispatchContext);
  if (!dispatch) throw new Error('useAppDispatch 必须在 AppProvider 内使用');
  return dispatch;
}
