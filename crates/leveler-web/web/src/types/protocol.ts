//! 协议类型 —— 与 crates/leveler-client-protocol 的 serde JSON 形状逐字段对齐。
//!
//! 规则：`tag = "type"` + `rename_all = "snake_case"` 的 tagged union；
//! 所有 id 类型（SessionId / MessageId / ToolCallId / ApprovalId /
//! ClarificationId / CheckpointId / AttachmentId / CommandId）在 wire 上都是
//! 透明字符串（leveler-core `string_id!` newtype）。

// ── id ──────────────────────────────────────────────────────────────
export type SessionId = string;
export type MessageId = string;
export type ToolCallId = string;
export type ApprovalId = string;
export type ClarificationId = string;
export type CheckpointId = string;
export type AttachmentId = string;
export type CommandId = string;

// ── leveler-model ───────────────────────────────────────────────────
/** ModelRef { provider, model }（leveler-model/src/request.rs） */
export interface ModelRef {
  provider: string;
  model: string;
}

// ── wire_types.rs ───────────────────────────────────────────────────
export type ApprovalDecision =
  | 'approve_once'
  | 'approve_session'
  | 'approve_always'
  | 'deny';

export type PermissionProfile = 'request_approval' | 'assisted' | 'full_access';

// ── media.rs ────────────────────────────────────────────────────────
export type AttachmentKind = 'image' | 'text_file' | 'document' | 'unknown';

export interface AttachmentRef {
  id: AttachmentId;
  kind: AttachmentKind;
  name: string;
  mime_type: string;
  size_bytes: number;
  sha256: string;
  width: number | null;
  height: number | null;
}

// ── snapshot.rs ─────────────────────────────────────────────────────
export type UiRole = 'user' | 'assistant' | 'system' | 'tool';

export interface UiMessage {
  id: MessageId;
  role: UiRole;
  text: string;
}

export interface UiCheckpoint {
  id: CheckpointId;
  label: string;
  ordinal: number;
}

export interface UiActiveToolCall {
  id: ToolCallId;
  name: string;
  arguments: string;
}

export interface UiSessionSummary {
  id: SessionId;
  goal: string;
  status: string;
  model: string;
  updated_at: string;
  /**
   * Rust 端 UiSessionSummary 目前没有 repository 字段；多项目（任意 repo
   * root）不在本期。此处预留：协议一旦补上，分组逻辑自动生效。
   */
  repository?: string;
}

// ── progress.rs ─────────────────────────────────────────────────────
export type PlanStepStatus = 'pending' | 'running' | 'done' | 'failed' | 'skipped';

export interface UiPlanStep {
  index: number;
  description: string;
  status: PlanStepStatus;
}

export interface UiPlan {
  steps: UiPlanStep[];
}

export type CheckState = 'running' | 'passed' | 'failed' | 'skipped';

export interface UiCheck {
  name: string;
  status: CheckState;
  evidence: string | null;
}

export interface UiVerification {
  checks: UiCheck[];
  passed: boolean | null;
}

export interface UiDiffFile {
  path: string;
  added: number;
  removed: number;
  patch: string | null;
}

export interface UiDiff {
  files: UiDiffFile[];
}

export interface UiCompletionReport {
  files_changed: number;
  added: number;
  removed: number;
  checks_passed: number;
  checks_total: number;
  success: boolean;
}

// ── approval.rs ─────────────────────────────────────────────────────
export interface UiApprovalRequest {
  id: ApprovalId;
  tool: string;
  summary: string;
  command: string | null;
  risks: string[];
}

export interface UiClarificationRequest {
  id: ClarificationId;
  question: string;
  options: string[];
}

/** `tag = "type", content = "request"` */
export type UiPendingInteraction =
  | { type: 'approval'; request: UiApprovalRequest }
  | { type: 'clarification'; request: UiClarificationRequest };

// ── UiSessionSnapshot（snapshot.rs；defaulted 字段标可选）────────────
export interface UiSessionSnapshot {
  id: SessionId;
  repository: string;
  goal: string;
  model: ModelRef | null;
  mode: PermissionProfile;
  branch: string | null;
  status: string;
  messages: UiMessage[];
  pending_interactions?: UiPendingInteraction[];
  available_models?: ModelRef[];
  vision?: boolean;
  last_sequence?: number | null;
  active_tools?: UiActiveToolCall[];
  plan?: UiPlan | null;
  verification?: UiVerification | null;
  diff?: UiDiff | null;
  checkpoints?: UiCheckpoint[];
  completion_report?: UiCompletionReport | null;
}

// ── command.rs：ClientCommand ───────────────────────────────────────
export type ClientCommand =
  | { type: 'submit_message'; session_id: SessionId; content: string; attachments?: AttachmentRef[] }
  | { type: 'run_goal'; session_id: SessionId; content: string }
  | { type: 'add_attachment'; session_id: SessionId; path: string }
  | { type: 'add_clipboard_image'; session_id: SessionId }
  | { type: 'cancel_current_turn'; session_id: SessionId }
  | { type: 'force_cancel_current_turn'; session_id: SessionId }
  | { type: 'approval_decision'; request_id: ApprovalId; decision: ApprovalDecision }
  | { type: 'answer_clarification'; request_id: ClarificationId; answer: string }
  | { type: 'select_model'; session_id: SessionId; model: ModelRef }
  | { type: 'set_permission_profile'; session_id: SessionId; mode: PermissionProfile }
  | { type: 'set_product_axes'; session_id: SessionId; work_profile: string; collaboration: string }
  | { type: 'confirm_plan_to_goal'; session_id: SessionId; content: string }
  | { type: 'list_memory'; session_id: SessionId; include_archived?: boolean }
  | { type: 'forget_memory'; session_id: SessionId; id: string }
  | { type: 'set_agent_mode'; session_id: SessionId; orchestrate: boolean }
  | { type: 'request_diff'; session_id: SessionId }
  | { type: 'compact_context'; session_id: SessionId }
  | { type: 'clear_conversation'; session_id: SessionId }
  | { type: 'request_session_list' }
  | { type: 'request_session_list_for'; requester_session_id: SessionId }
  | { type: 'open_session'; session_id: SessionId }
  | { type: 'open_session_for'; requester_session_id: SessionId; session_id: SessionId }
  | { type: 'delete_session'; session_id: SessionId }
  | { type: 'delete_session_for'; requester_session_id: SessionId; session_id: SessionId }
  | { type: 'restore_checkpoint'; session_id: SessionId; checkpoint_id: CheckpointId }
  | { type: 'btw'; session_id: SessionId; question: string }
  | { type: 'quit' };

// ── event.rs：RuntimeEvent ──────────────────────────────────────────
export type NotificationLevel = 'info' | 'warning' | 'error';

export interface UiMemoryEntry {
  id: string;
  title: string;
}

export type RuntimeEvent =
  | { type: 'runtime_ready' }
  | { type: 'session_opened'; session: UiSessionSnapshot }
  | { type: 'session_updated'; session: UiSessionSnapshot }
  | { type: 'approval_requested'; request: UiApprovalRequest }
  | { type: 'clarification_requested'; request: UiClarificationRequest }
  | { type: 'attachment_added'; attachment: AttachmentRef }
  | { type: 'attachment_processing_failed'; error: string }
  | { type: 'user_message_added'; message: UiMessage }
  | { type: 'assistant_message_started'; message_id: MessageId }
  | { type: 'assistant_attempt_reset'; message_id: MessageId | null }
  | { type: 'assistant_text_delta'; message_id: MessageId; delta: string }
  | { type: 'reasoning_delta'; delta: string }
  | { type: 'assistant_message_completed'; message_id: MessageId }
  | { type: 'agent_activity'; label: string }
  | { type: 'project_rules_loaded'; sources: string[] }
  | { type: 'tool_call_started'; id: ToolCallId; name: string; arguments: string; parallel?: boolean }
  | { type: 'tool_call_completed'; id: ToolCallId; ok: boolean; preview: string; duration_ms: number }
  | { type: 'plan_updated'; plan: UiPlan }
  | { type: 'verification_updated'; verification: UiVerification }
  | { type: 'diff_updated'; diff: UiDiff }
  | { type: 'checkpoint_created'; checkpoint: UiCheckpoint }
  | { type: 'session_list'; sessions: UiSessionSummary[] }
  | { type: 'context_updated'; candidate_files: string[]; estimated_tokens: number }
  | { type: 'token_usage'; input_tokens: number; output_tokens: number; cached_input_tokens: number }
  | { type: 'session_completed'; report: UiCompletionReport }
  | { type: 'turn_completed' }
  | { type: 'turn_answered' }
  | { type: 'turn_truncated'; error: string }
  | { type: 'turn_incomplete'; reason: string }
  | { type: 'turn_completed_unverified'; reason: string }
  | { type: 'turn_failed'; error: string }
  | { type: 'turn_cancelled' }
  | { type: 'sub_agent_updated'; id: string; nickname: string; role: string; done: boolean; ok: boolean; detail: string }
  | { type: 'sub_agent_progress'; id: string; active: boolean; input_tokens: number; output_tokens: number; cached_input_tokens: number }
  | { type: 'notification'; level: NotificationLevel; message: string }
  | { type: 'background_task_started'; task_id: string; program: string; args: string[] }
  | { type: 'background_task_exited'; task_id: string; exit_code: number | null; duration_ms: number; ok: boolean }
  | { type: 'memory_list'; memory_dir: string; active: UiMemoryEntry[]; archived: UiMemoryEntry[] }
  | { type: 'btw_started'; question: string }
  | { type: 'btw_text_delta'; delta: string }
  | { type: 'btw_completed' }
  | { type: 'btw_failed'; error: string }
  | { type: 'turn_progress'; phase: string; closing: boolean; no_progress_streak: number; closeout_deny_rounds: number };

/** turn 终态事件集合（驱动消息队列出队）。 */
export const TURN_TERMINAL_TYPES: ReadonlySet<RuntimeEvent['type']> = new Set([
  'turn_completed',
  'turn_answered',
  'turn_truncated',
  'turn_incomplete',
  'turn_completed_unverified',
  'turn_failed',
  'turn_cancelled',
]);

// ── WS 帧（与 leveler-web 网关的契约，见 design/ 说明）───────────────
/** 上行：浏览器 → 服务端。 */
export type UpFrame =
  | { type: 'deliver'; command_id: CommandId; session_id: SessionId; command: ClientCommand }
  | { type: 'snapshot'; session_id: SessionId };

/** 下行：服务端 → 浏览器。 */
export type DownFrame =
  | { type: 'event'; event: RuntimeEvent }
  | { type: 'snapshot'; session: UiSessionSnapshot }
  | { type: 'ack'; command_id: CommandId }
  | { type: 'error'; code: string; message: string; command_id: CommandId | null }
  | { type: 'resync_required'; session_id: SessionId };

// ── leveler-local-transport：REST DTO ───────────────────────────────
/** CreateSessionRequest（leveler-local-transport/src/lib.rs） */
export interface CreateSessionRequest {
  goal: string;
  model: ModelRef | null;
  mode: PermissionProfile;
}

/** SessionBootstrap = POST /api/sessions 的响应。 */
export interface SessionBootstrap {
  session: UiSessionSnapshot;
  context_window: number;
}
