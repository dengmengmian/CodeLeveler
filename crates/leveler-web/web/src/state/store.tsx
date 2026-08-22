// 应用状态：useReducer 单向数据流。
// 视图模型对齐三栏结构；数据源全部来自协议契约
// （UiSessionSnapshot 整量 + RuntimeEvent 增量）。
// 语义规则与 TUI reducer（crates/leveler-tui/src/reducer/runtime_apply.rs）
// 保持同构：同一事件在两个 UI 里表达同一产品事实。

import { createContext, useContext, type Dispatch, type ReactNode } from 'react';
import { isCompactionSummaryText, isTurnUser } from '../lib/presentationKind';
import { useImmerReducer } from '../lib/useImmerReducer';
import type { TurnOutcome } from '../lib/turn';
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
  UiMemoryEntry,
  UiPlan,
  UiRole,
  UiObservabilityLoaded,
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
  /** 旁问（/btw）侧答：存被问的问题，非空即渲染为独立侧答卡片 */
  btw?: string;
  /** Product presentation; compaction is stamped by the runtime prefix. */
  kind?: 'compaction_summary';
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

/** 上一回合终态：7 个 runtime 终态逐一保真（Turn Truth），detail 永不丢。 */
export interface LastTurn {
  outcome: TurnOutcome;
  ms: number;
  detail: string | null;
}

/** Frozen per-turn execution process. Live `tools` still clear on the next user message. */
export interface TurnTrace {
  userSeq: number;
  tools: ToolCallView[];
  backgroundTasks: BackgroundTaskView[];
  lastTurn: LastTurn;
}

/** 一个 spawn 出来的子 agent（多 agent 委派），running → done 原地更新。 */
export interface SubAgentView {
  id: string;
  nickname: string;
  role: string;
  status: 'run' | 'done' | 'fail';
  /** 运行中 = 任务描述；完成后 = 结果摘要（协议语义） */
  detail: string;
  /** 最近一步工具活动（sub_agent_activity），如 `cargo test ✓` */
  recentStep: string | null;
  active: boolean;
  tokens: { input: number; output: number; cached: number };
  seq: number;
}

/** 后台任务（run_command background=true）：当前 Agent 的执行活动。 */
export interface BackgroundTaskView {
  id: string;
  program: string;
  status: 'run' | 'done' | 'fail';
  exitCode: number | null;
  durationMs: number | null;
  startedAt: number;
}

/** 项目记忆（memory_list）：active / pending（待用户采纳）/ archived。 */
export interface MemoryView {
  dir: string;
  active: UiMemoryEntry[];
  archived: UiMemoryEntry[];
  pending: UiMemoryEntry[];
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
  /** Completed turns' tool process, keyed by that turn's user message seq. */
  traces: TurnTrace[];
  /** 当前回合的子 agent（下一回合开始时清空） */
  agents: SubAgentView[];
  /** 当前回合的后台任务（下一回合开始时清空） */
  backgroundTasks: BackgroundTaskView[];
  pendingApprovals: UiApprovalRequest[];
  pendingClarifications: UiClarificationRequest[];
  plan: UiPlan | null;
  verification: UiVerification | null;
  diff: UiDiff | null;
  checkpoints: UiCheckpoint[];
  completionReport: UiCompletionReport | null;
  memory: MemoryView | null;
  turnActive: boolean;
  /** agent_activity / command_progress / turn_progress 的统一 coarse activity */
  activity: string | null;
  /** 模型推理流（reasoning_delta）；工具调用后被下一条 delta 替换（TUI 同款） */
  reasoning: string;
  reasoningSuperseded: boolean;
  /** 当前回合开始时间（epoch ms）；空闲时为 null，用于运行计时 */
  turnStartedAt: number | null;
  /** 上一回合终态（7 值保真）；新回合开始时清空 */
  lastTurn: LastTurn | null;
  model: ModelRef | null;
  availableModels: ModelRef[];
  permission: PermissionProfile;
  /** 产品轴（economy|balanced|delivery）；SoT 是 session record，snapshot 带回 */
  workProfile: string;
  /** 产品轴（chat|plan|goal）；goal 时 runtime 把普通提交路由成 goal turn */
  collaboration: string;
  /** runtime 决议后的 reasoning effort（snapshot.reasoning.effective），只展示不发明 */
  reasoningEffort: string | null;
  tokens: { input: number; output: number };
  /** 上下文占用（token_usage 的 input+output；无真实读数时用估算占位） */
  contextTokens: number;
  /** 来自 SessionBootstrap；不知道时 CTX 表按 0% */
  contextWindow: number | null;
}

export type ConnectionStatus = 'connecting' | 'online';

/** Central workspace surface. `execution` is a Phase 1 slot only (no observatory). */
export type StageView = 'chat' | 'diff' | 'execution';

/** Sidebar destination. Workspace tabs (Conversation / Changes / Execution) are independent. */
export type RailNav = 'sessions' | 'files' | 'search' | 'settings';

/** Sections inside the Workspace sidebar. Symbols/Environment have no extra API. */
export type WorkspaceSection = 'files' | 'symbols' | 'repository' | 'environment';

export interface AppState {
  connection: ConnectionStatus;
  sessions: UiSessionSummary[];
  current: SessionView | null;
  /** true = 空状态 hero（新对话，尚未建会话） */
  draft: boolean;
  /** 当前 runtime 的仓库路径（分组回退值 + hero 项目选择器） */
  repository: string;
  /** 中央主区域视图（对话 / 改动 / Execution 占位） */
  stageView: StageView;
  /** Single-sidebar destination. Independent of workspace tabs. */
  railNav: RailNav;
  /** Workspace sidebar subsection. */
  workspaceSection: WorkspaceSection;
  queue: QueuedMessage[];
  notice: string | null;
  /** 已上传、待随下一条消息提交的附件 */
  pendingAttachments: AttachmentRef[];
  /** 聚合层注册的项目（含状态）；空数组表示尚未拉取或单项目模式 */
  projects: ProjectInfo[];
  /** 新对话的目标项目（= 项目分组上的 ＋ 入口）；null = 当前仓库 */
  draftProject: string | null;
  /** Selected Project identity: canonical repository path. */
  selectedProject: string | null;
  /** 待注入到输入框的文本（空状态快捷操作 → Composer 消费后清空） */
  composerSeed: string | null;
  /** Diff 工作区当前聚焦的文件；null = 用列表第一项 */
  diffFocus: string | null;
  /** Single sidebar open. */
  railOpen: boolean;
  inspectorOpen: boolean;
  /** Open the Inspector More disclosure (Checkpoints / Memory). */
  inspectorMore: boolean;
  /** Durable observatory payload (QueryObservability). Not live SessionView.tools. */
  observation: UiObservabilityLoaded | null;
  observationStatus: 'idle' | 'loading' | 'ready' | 'error';
  /** QueryObservability.query_id this view currently owns. */
  pendingObservationQuery: string | null;
}

export const initialState: AppState = {
  connection: 'connecting',
  sessions: [],
  current: null,
  draft: true,
  repository: '',
  stageView: 'chat',
  railNav: 'sessions',
  workspaceSection: 'files',
  queue: [],
  notice: null,
  pendingAttachments: [],
  projects: [],
  draftProject: null,
  selectedProject: null,
  composerSeed: null,
  diffFocus: null,
  railOpen: true,
  inspectorOpen: true,
  inspectorMore: false,
  observation: null,
  observationStatus: 'idle',
  pendingObservationQuery: null,
};

// ── Actions ─────────────────────────────────────────────────────────

export type Action =
  | { type: 'connection'; status: ConnectionStatus }
  | { type: 'session_list'; sessions: UiSessionSummary[] }
  | { type: 'snapshot'; session: UiSessionSnapshot; contextWindow?: number | null }
  /** SessionUpdated: TUI apply_meta. Metadata only; never rebuilds the turn presentation. */
  | { type: 'session_meta'; session: UiSessionSnapshot }
  | { type: 'select_session'; id: SessionId }
  | { type: 'new_draft'; project?: string | null }
  | { type: 'select_project'; path: string }
  | { type: 'stage_view'; view: StageView }
  | { type: 'set_rail_nav'; nav: RailNav }
  | { type: 'set_workspace_section'; section: WorkspaceSection }
  | { type: 'focus_diff'; path: string | null }
  | { type: 'toggle_rail' }
  | { type: 'toggle_inspector' }
  | { type: 'set_inspector'; open: boolean }
  | { type: 'set_inspector_more'; open: boolean }
  | { type: 'observation_loading'; queryId: string }
  | { type: 'observation_loaded'; observation: UiObservabilityLoaded; queryId: string | null }
  | { type: 'user_message'; id: string; text: string; time: string }
  | { type: 'assistant_started'; id: string; time: string }
  | { type: 'assistant_reset'; id: string | null }
  | { type: 'assistant_delta'; id: string; delta: string }
  | { type: 'assistant_completed'; id: string }
  | { type: 'reasoning_delta'; delta: string }
  | { type: 'btw_started'; question: string; time: string }
  | { type: 'btw_delta'; delta: string }
  | { type: 'btw_done' }
  | { type: 'tool_started'; id: ToolCallId; name: string; arguments: string; parallel: boolean }
  | { type: 'tool_completed'; id: ToolCallId; ok: boolean; preview: string; durationMs: number }
  | { type: 'sub_agent_updated'; id: string; nickname: string; role: string; done: boolean; ok: boolean; detail: string }
  | { type: 'sub_agent_progress'; id: string; active: boolean; input: number; output: number; cached: number }
  | { type: 'sub_agent_activity'; id: string; step: string }
  | { type: 'background_started'; taskId: string; program: string; args: string[] }
  | { type: 'background_exited'; taskId: string; exitCode: number | null; durationMs: number; ok: boolean }
  | { type: 'memory_list'; dir: string; active: UiMemoryEntry[]; archived: UiMemoryEntry[]; pending: UiMemoryEntry[] }
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
  | { type: 'context_estimate'; tokens: number }
  | { type: 'turn_active'; value: boolean }
  | { type: 'turn_terminal'; outcome: TurnOutcome; detail?: string | null }
  | { type: 'seed_composer'; text: string | null }
  | { type: 'enqueue'; item: QueuedMessage }
  | { type: 'dequeue'; id: string }
  | { type: 'queue_move'; id: string; dir: -1 | 1 }
  | { type: 'set_permission'; mode: PermissionProfile }
  | { type: 'set_model'; model: ModelRef }
  | { type: 'set_axes'; workProfile: string; collaboration: string }
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
  // Full replace (SessionOpened / WS snapshot). SessionUpdated must NOT use this:
  // active_tools is only in-flight, and rebuilding here wipes the current turn.
  const messages: ChatMessage[] = snap.messages.map((m) => ({
    id: m.id,
    role: m.role,
    text: m.text,
    streaming: false,
    time: null,
    seq: nextSeq(),
    kind:
      m.role === 'user' && isCompactionSummaryText(m.text) ? 'compaction_summary' : undefined,
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
    branch: snap.branch ?? null,
    status: snap.status,
    messages,
    tools,
    traces: sameSession ? (prev.traces ?? []) : [],
    agents: sameSession ? prev.agents : [],
    backgroundTasks: sameSession ? prev.backgroundTasks : [],
    pendingApprovals,
    pendingClarifications,
    plan: snap.plan ?? null,
    verification: snap.verification ?? null,
    diff: snap.diff ?? null,
    checkpoints: snap.checkpoints ?? [],
    completionReport: snap.completion_report ?? null,
    memory: sameSession ? prev.memory : null,
    turnActive,
    activity: sameSession ? prev.activity : null,
    reasoning: sameSession ? prev.reasoning : '',
    reasoningSuperseded: sameSession ? prev.reasoningSuperseded : false,
    turnStartedAt: sameSession ? prev.turnStartedAt : turnActive ? Date.now() : null,
    lastTurn: sameSession ? prev.lastTurn : null,
    model: snap.model ?? null,
    availableModels: snap.available_models ?? [],
    permission: snap.mode,
    // 轴的 SoT 是 session record；老 runtime 不带字段时保留本地值 / 回退默认。
    workProfile: snap.work_profile ?? (sameSession ? prev.workProfile : 'balanced'),
    collaboration: snap.collaboration ?? (sameSession ? prev.collaboration : 'chat'),
    // reasoning.effective 缺席 = 老 runtime（保留旧值）；present but null =
    // 模型没有可控档位（就是 null，不发明）。
    reasoningEffort:
      snap.reasoning !== undefined && snap.reasoning !== null
        ? (snap.reasoning.effective ?? null)
        : sameSession
          ? prev.reasoningEffort
          : null,
    tokens: sameSession ? prev.tokens : { input: 0, output: 0 },
    contextTokens: sameSession ? prev.contextTokens : 0,
    contextWindow: contextWindow ?? (sameSession ? prev.contextWindow : null),
  };
}

/** Drop every projection that belongs to the session currently on screen. */
function leaveSessionView(state: AppState): void {
  state.current = null;
  state.diffFocus = null;
  state.observation = null;
  state.observationStatus = 'idle';
  state.pendingObservationQuery = null;
  state.pendingAttachments = [];
  state.composerSeed = null;
}

/** TUI mark_turn_busy 的对应物：事件到来说明回合在跑。 */
function markBusy(current: SessionView): void {
  if (!current.turnActive) {
    current.turnActive = true;
    current.turnStartedAt = Date.now();
    current.lastTurn = null;
  }
}

/** TUI `apply_meta`: header fields only. Does not touch transcript, tools, agents, lastTurn. */
function applySessionMeta(current: SessionView, snap: UiSessionSnapshot): void {
  current.repository = snap.repository;
  current.branch = snap.branch ?? null;
  current.model = snap.model ?? null;
  current.availableModels = snap.available_models ?? [];
  current.permission = snap.mode;
  if (snap.status) current.status = snap.status;
  if (snap.goal) current.title = snap.goal || current.title;
  if (snap.work_profile) current.workProfile = snap.work_profile;
  if (snap.collaboration) current.collaboration = snap.collaboration;
  if (snap.reasoning !== undefined && snap.reasoning !== null) {
    current.reasoningEffort = snap.reasoning.effective ?? null;
  }
}

function resetReasoning(current: SessionView): void {
  current.reasoning = '';
  current.reasoningSuperseded = false;
}

function snapshotTurnTrace(current: SessionView): void {
  if (!current.lastTurn) return;
  if (!current.traces) current.traces = [];
  let userSeq: number | null = null;
  for (let i = current.messages.length - 1; i >= 0; i -= 1) {
    const m = current.messages[i];
    if (isTurnUser(m)) {
      userSeq = m.seq;
      break;
    }
  }
  if (userSeq === null) return;
  const trace: TurnTrace = {
    userSeq,
    tools: current.tools.slice(),
    backgroundTasks: current.backgroundTasks.slice(),
    lastTurn: current.lastTurn,
  };
  const idx = current.traces.findIndex((t) => t.userSeq === userSeq);
  if (idx >= 0) current.traces[idx] = trace;
  else current.traces.push(trace);
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
      if (view.repository) {
        state.repository = view.repository;
        state.selectedProject = view.repository;
      }
      return;
    }
    case 'session_meta': {
      if (!state.current || state.current.id !== action.session.id) return;
      applySessionMeta(state.current, action.session);
      if (state.current.repository) {
        state.repository = state.current.repository;
        state.selectedProject = state.current.repository;
      }
      return;
    }
    case 'select_project':
      if (state.selectedProject === action.path && state.draftProject === action.path) {
        return;
      }
      state.selectedProject = action.path;
      state.draftProject = action.path;
      if (state.current && state.current.repository !== action.path) {
        state.draft = true;
        leaveSessionView(state);
      } else if (!state.current) {
        state.draft = true;
      }
      return;
    case 'select_session':
      state.draft = false;
      if (state.current?.id !== action.id) {
        leaveSessionView(state); // 等 snapshot
      }
      return;
    case 'new_draft':
      state.draft = true;
      state.draftProject = action.project ?? state.selectedProject;
      if (state.draftProject) state.selectedProject = state.draftProject;
      leaveSessionView(state);
      return;
    case 'stage_view':
      state.stageView = action.view;
      return;
    case 'set_rail_nav':
      state.railNav = action.nav;
      return;
    case 'set_workspace_section':
      state.workspaceSection = action.section;
      return;
    case 'focus_diff':
      state.stageView = 'diff';
      state.diffFocus = action.path;
      return;
    case 'toggle_rail':
      state.railOpen = !state.railOpen;
      return;
    case 'toggle_inspector':
      state.inspectorOpen = !state.inspectorOpen;
      return;
    case 'set_inspector':
      state.inspectorOpen = action.open;
      return;
    case 'set_inspector_more':
      state.inspectorMore = action.open;
      if (action.open) state.inspectorOpen = true;
      return;
    case 'observation_loading':
      state.observationStatus = 'loading';
      state.pendingObservationQuery = action.queryId;
      return;
    case 'observation_loaded':
      if (state.current && action.observation.session.session_id !== state.current.id) {
        return;
      }
      if (
        state.pendingObservationQuery == null ||
        action.queryId == null ||
        state.pendingObservationQuery !== action.queryId
      ) {
        return;
      }
      state.observation = action.observation;
      state.observationStatus = 'ready';
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
      // 新回合开始：清掉上一回合的执行轨（工具/子 agent/后台任务）与终态
      state.current.tools = [];
      state.current.agents = [];
      state.current.backgroundTasks = [];
      state.current.turnActive = true;
      state.current.turnStartedAt = Date.now();
      state.current.activity = null;
      state.current.lastTurn = null;
      resetReasoning(state.current);
      return;
    }
    case 'assistant_started': {
      if (!state.current) return;
      markBusy(state.current);
      resetReasoning(state.current);
      if (state.current.messages.some((m) => m.id === action.id)) return;
      state.current.messages.push({
        id: action.id,
        role: 'assistant',
        text: '',
        streaming: true,
        time: action.time,
        seq: nextSeq(),
      });
      return;
    }
    case 'assistant_reset': {
      if (!state.current) return;
      resetReasoning(state.current);
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
    case 'reasoning_delta': {
      if (!state.current) return;
      markBusy(state.current);
      // 上一步的思考在它调用工具时就结束了；这条 delta 属于新的一步，
      // 替换而不是续写（TUI 同款规则）。
      if (state.current.reasoningSuperseded) resetReasoning(state.current);
      state.current.reasoning += action.delta;
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
        btw: action.question,
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
      markBusy(state.current);
      // 对思考采取行动 = 思考结束；下一条 reasoning delta 会替换它。
      state.current.reasoningSuperseded = true;
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
      // 工具结束后把它的 activity 标签留着会像挂死；退回思考态（TUI 同款）。
      if (state.current) {
        state.current.activity = null;
        resetReasoning(state.current);
      }
      return;
    }
    case 'sub_agent_updated': {
      if (!state.current) return;
      markBusy(state.current);
      const existing = state.current.agents.find((a) => a.id === action.id);
      if (existing) {
        existing.nickname = action.nickname;
        existing.role = action.role;
        existing.detail = action.detail;
        if (action.done) {
          existing.status = action.ok ? 'done' : 'fail';
          existing.active = false;
        }
        return;
      }
      state.current.agents.push({
        id: action.id,
        nickname: action.nickname,
        role: action.role,
        status: action.done ? (action.ok ? 'done' : 'fail') : 'run',
        detail: action.detail,
        recentStep: null,
        active: !action.done,
        tokens: { input: 0, output: 0, cached: 0 },
        seq: nextSeq(),
      });
      return;
    }
    case 'sub_agent_progress': {
      const agent = state.current?.agents.find((a) => a.id === action.id);
      if (agent) {
        agent.active = action.active;
        agent.tokens = { input: action.input, output: action.output, cached: action.cached };
      }
      return;
    }
    case 'sub_agent_activity': {
      const agent = state.current?.agents.find((a) => a.id === action.id);
      if (agent) agent.recentStep = action.step;
      return;
    }
    case 'background_started': {
      if (!state.current) return;
      markBusy(state.current);
      if (state.current.backgroundTasks.some((t) => t.id === action.taskId)) return;
      state.current.backgroundTasks.push({
        id: action.taskId,
        program: [action.program, ...action.args].join(' '),
        status: 'run',
        exitCode: null,
        durationMs: null,
        startedAt: Date.now(),
      });
      return;
    }
    case 'background_exited': {
      const task = state.current?.backgroundTasks.find((t) => t.id === action.taskId);
      if (task) {
        task.status = action.ok ? 'done' : 'fail';
        task.exitCode = action.exitCode;
        task.durationMs = action.durationMs;
      }
      return;
    }
    case 'memory_list':
      if (state.current) {
        state.current.memory = {
          dir: action.dir,
          active: action.active,
          archived: action.archived,
          pending: action.pending,
        };
      }
      return;
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
        // 全零读数不覆盖已有数据（provider 缺 usage chunk 时，TUI 同款保护）。
        if (action.input === 0 && action.output === 0) return;
        state.current.tokens = { input: action.input, output: action.output };
        // input 是本轮完整 prompt，加 output 即回复后的窗口占用；取代不累加。
        state.current.contextTokens = action.input + action.output;
      }
      return;
    case 'context_estimate':
      // 预运行估算只做占位：有真实 usage 后不许覆盖，避免表针抖动。
      if (state.current && state.current.contextTokens === 0 && state.current.tokens.input === 0) {
        state.current.contextTokens = action.tokens;
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
          detail: action.detail ?? null,
        };
        state.current.turnActive = false;
        state.current.turnStartedAt = null;
        state.current.activity = null;
        resetReasoning(state.current);
        for (const m of state.current.messages) m.streaming = false;
        snapshotTurnTrace(state.current);
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
    case 'set_axes':
      if (state.current) {
        state.current.workProfile = action.workProfile;
        state.current.collaboration = action.collaboration;
      }
      return;
    case 'agent_activity':
      if (state.current) {
        markBusy(state.current);
        state.current.activity = action.label;
      }
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
    case 'projects': {
      state.projects = action.projects;
      if (action.projects.length === 0) {
        if (!state.selectedProject) {
          state.selectedProject =
            state.current?.repository ||
            state.draftProject ||
            state.repository ||
            null;
        }
        return;
      }
      const listed = action.projects.some((p) => p.path === state.selectedProject);
      if (!state.selectedProject || !listed) {
        const fromSession = action.projects.find((p) => p.path === state.current?.repository)?.path;
        const next = fromSession ?? action.projects[0].path;
        state.selectedProject = next;
        state.draftProject = next;
        if (state.current && state.current.repository !== next) {
          state.draft = true;
          leaveSessionView(state);
        }
      }
      return;
    }
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
