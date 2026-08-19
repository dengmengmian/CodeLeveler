// 自动生成，禁止手改 —— npm run gen:protocol 重新生成。
// 事实源：Rust crates/leveler-client-protocol → schemas/*.schema.json
// （schema 由 `UPDATE_SCHEMAS=1 cargo test -p leveler-client-protocol --features schema` 守护）。
// web 网关自有帧（UpFrame/DownFrame/REST DTO）不在此文件，见 protocol.ts。

export type ApprovalDecision = 'approve_once' | 'approve_session' | 'approve_always' | 'deny';

/** Identifies a pending permission approval request. */
export type ApprovalId = string;

/** Identifies a pending or sent attachment. */
export type AttachmentId = string;

/** The kind of attachment. */
export type AttachmentKind = 'image' | 'text_file' | 'document' | 'unknown';

/** A reference to a processed, stored attachment (spec §39). Carries only metadata; the bytes are addressed by `sha256` in the media store. */
export interface AttachmentRef {
  height?: number | null;
  id: AttachmentId;
  kind: AttachmentKind;
  mime_type: string;
  name: string;
  /** Content-address of the processed bytes in the media store. */
  sha256: string;
  size_bytes: number;
  width?: number | null;
}

/** Identifies a conversation checkpoint (restore point). */
export type CheckpointId = string;

/** The state of one verification check. */
export type CheckState = 'running' | 'passed' | 'failed' | 'skipped';

/** Identifies a pending clarification (ask-user) request. */
export type ClarificationId = string;

/** Identifies a single assistant/user message in the transcript. A protocol-level id (the runtime persists messages as an ordered log, not by id); it lets streaming deltas target the right in-flight message. */
export type MessageId = string;

/** A provider + model pair. The rest of the system routes on this, never on a bare model-name string. */
export interface ModelRef {
  model: string;
  provider: string;
}

/** Severity for a transient notification . */
export type NotificationLevel = 'info' | 'warning' | 'error';

/** Presentation class for a meaningful durable event. Unknown tools map to [`Self::Tool`]. */
export type ObservationClass = 'model' | 'read' | 'search' | 'edit' | 'shell' | 'tool' | 'verify' | 'agent' | 'recovery' | 'system' | 'terminal';

export type PermissionProfile = 'request_approval' | 'assisted' | 'full_access';

/** The lifecycle state of a plan step (mirrors the orchestrator's `NodeStatus`). */
export type PlanStepStatus = 'pending' | 'running' | 'done' | 'failed' | 'skipped';

/** Identifies a single agent session (one user goal end to end). */
export type SessionId = string;

/** Identifies a tool call. Must be stable across streaming reassembly. */
export type ToolCallId = string;

/** A tool invocation that was still running when a client took its snapshot. */
export interface UiActiveToolCall {
  arguments: string;
  id: ToolCallId;
  name: string;
}

/** Durable sub-agent / reviewer observation. */
export interface UiAgentObservation {
  id: string;
  nickname: string;
  role: string;
  status: string;
  summary?: string;
}

/** A pending permission request, projected for display. */
export interface UiApprovalRequest {
  /** The concrete command, when the tool is `run_command`. */
  command?: string | null;
  id: ApprovalId;
  /** Human-readable risk bullets (paths touched, network, etc.). */
  risks: string[];
  /** A one-line summary of what will happen. */
  summary: string;
  /** The tool requesting permission (e.g. `run_command`). */
  tool: string;
}

/** One verification check (spec §22). */
export interface UiCheck {
  /** Captured evidence (command output), for failures. */
  evidence?: string | null;
  name: string;
  status: CheckState;
}

/** A conversation restore point (spec §68). Restoring truncates the transcript back to `ordinal` messages; working-tree files are left to the user's git. */
export interface UiCheckpoint {
  id: CheckpointId;
  label: string;
  /** The persisted-message count to truncate back to. */
  ordinal: number;
}

/** A mid-task clarification the agent needs answered (spec §35). */
export interface UiClarificationRequest {
  id: ClarificationId;
  /** Candidate answers, when the model offered a choice. */
  options: string[];
  question: string;
}

/** The final completion report (spec §23). */
export interface UiCompletionReport {
  added: number;
  checks_passed: number;
  checks_total: number;
  files_changed: number;
  removed: number;
  /** Whether the run completed and verified successfully. */
  success: boolean;
}

/** A summary of working-tree changes. */
export interface UiDiff {
  files: UiDiffFile[];
}

/** One changed file (spec §21). */
export interface UiDiffFile {
  added: number;
  /** The unified diff hunk text, loaded on demand. */
  patch?: string | null;
  path: string;
  removed: number;
}

/** Identity-based relation (never inferred from wall-clock proximity). */
export interface UiEventRelation {
  /** pair_start | pair_end | same_turn | same_agent */
  kind: string;
  label: string;
  sequence: number;
}

/** Compact durable-memory row for TUI list surfaces. */
export interface UiMemoryEntry {
  id: string;
  title: string;
}

/** A rendered message in the transcript. */
export interface UiMessage {
  id: MessageId;
  role: UiRole;
  text: string;
}

/** Bounded event window + related observation slices. Current and historical sessions use this same payload. */
export interface UiObservabilityLoaded {
  agents: UiAgentObservation[];
  recovery: UiRecoveryObservation;
  relations?: UiEventRelation[];
  requests: UiRequestObservation[];
  session: UiSessionObservation;
  tools: UiToolAggregate[];
  window: UiObservationRow[];
  window_from: number;
  window_to: number;
}

/** One inspect key/value. Avoids tuple arrays in the JSON schema. */
export interface UiObservationField {
  key: string;
  value: string;
}

/** One bounded, safe trace row. */
export interface UiObservationRow {
  class: ObservationClass;
  created_at: string;
  duration_ms?: number | null;
  /** Durable event type tag (`tool_call_finished`, …). */
  event_type: string;
  /** Safe inspect fields only (no raw args, no prompt, no secrets). */
  fields?: UiObservationField[];
  sequence: number;
  /** running | ok | fail | info */
  status: string;
  target?: string;
  title: string;
  turn_id?: string | null;
}

/** A live control request included in a reconnect snapshot. Only requests with an in-process waiter are projected; interrupted turns never resurrect stale buttons after a process restart. */
export type UiPendingInteraction =
  | { type: 'approval'; request: UiApprovalRequest }
  | { type: 'clarification'; request: UiClarificationRequest };

/** The execution plan (spec §20). */
export interface UiPlan {
  steps: UiPlanStep[];
}

/** One step in the execution plan. */
export interface UiPlanStep {
  description: string;
  index: number;
  status: PlanStepStatus;
}

/** Additive reasoning projection. The client must display `effective` and must not infer an effort from the model name or provider. */
export interface UiReasoningState {
  /** Wire value the runtime will send (`max`, `high`, …). */
  effective?: string | null;
}

/** Recovery facts that are already durable and safe to show. */
export interface UiRecoveryObservation {
  interrupted_turns: number;
  repair_attempts: number;
  review_stages: string[];
  workspace_snapshots: number;
}

/** One durable model-request row (no prompt/body). */
export interface UiRequestObservation {
  created_at: string;
  error_kind?: string | null;
  finish_reason?: string | null;
  id: string;
  input_tokens: number;
  latency_ms?: number | null;
  model: string;
  output_tokens: number;
  provider: string;
  retry_count: number;
}

/** Who authored a message. */
export type UiRole = 'user' | 'assistant' | 'system' | 'tool';

/** Session-level observation header + aggregates from durable stores. */
export interface UiSessionObservation {
  avg_latency_ms?: number | null;
  collaboration: string;
  compact_count: number;
  created_at: string;
  goal: string;
  input_tokens: number;
  last_latency_ms?: number | null;
  last_sequence?: number | null;
  model: string;
  output_tokens: number;
  repair_started: number;
  repository: string;
  request_count: number;
  request_failures: number;
  request_retries: number;
  session_id: SessionId;
  status: string;
  subagent_started: number;
  tool_finished: number;
  tool_started: number;
  updated_at: string;
  verification_runs: number;
  work_profile: string;
}

/** Everything a client needs to render a session's header and transcript. */
export interface UiSessionSnapshot {
  /** Live render state needed to reconnect while a long turn is still running. All fields are additive/defaulted for protocol compatibility. */
  active_tools?: UiActiveToolCall[];
  /** Models the user can switch to (for the model picker, ). */
  available_models?: ModelRef[];
  /** VCS branch, if the repository is a git repo. */
  branch?: string | null;
  checkpoints?: UiCheckpoint[];
  /** Product collaboration axis (`chat | plan | goal`). Same contract as `work_profile` — the runtime routes submits (goal) and restricts tools (plan) from this value, so clients must not invent it. */
  collaboration?: string | null;
  completion_report?: UiCompletionReport | null;
  diff?: UiDiff | null;
  goal: string;
  id: SessionId;
  /** The event-log sequence this snapshot reflects — the resync anchor. A client that fell behind (broadcast lag, reconnect) takes a fresh snapshot and resumes the event stream *after* this sequence, so it neither double-applies nor misses a canonical event. `None` when unknown (e.g. a brand-new session with no events yet). */
  last_sequence?: number | null;
  messages: UiMessage[];
  mode: PermissionProfile;
  model?: ModelRef | null;
  /** Live approval/clarification waiters for reconnect/resync. */
  pending_interactions?: UiPendingInteraction[];
  plan?: UiPlan | null;
  /** Runtime-projected reasoning state. Absent on old runtimes so a new client keeps its boot-time value. Present with `effective: None` means the model has no controllable effort knob (do not invent one). */
  reasoning?: UiReasoningState | null;
  repository: string;
  /** Persisted status string (e.g. "running", "completed"). */
  status: string;
  /** User shell executions: the active one (if any) plus a bounded recent history, newest last. Additive/defaulted like the rest of this block. */
  user_shells?: UiUserShell[];
  verification?: UiVerification | null;
  /** Whether the current model accepts image input (spec §42). */
  vision?: boolean;
  /** Product work-profile axis (`economy | balanced | delivery`). The source of truth is the session record (`SetProductAxes`); carried here so a reconnecting client shows the axis the runtime will actually use instead of a stale local guess. Absent on old runtimes. */
  work_profile?: string | null;
}

/** A one-line session summary for the Sessions screen (spec §52). */
export interface UiSessionSummary {
  goal: string;
  id: SessionId;
  model: string;
  /** Repository root the session belongs to. Filled by the runtime that owns the session and by the WebUI aggregation router (multi-project grouping); omitted on the wire when unknown so old fixtures and clients keep parsing. */
  repository?: string | null;
  status: string;
  updated_at: string;
}

/** Per-tool aggregate for the **whole session**, independent of the event window. Paired on `(call_id, agent_id)`; duration only from a matching start+finish. Unfinished starts are not success and do not invent duration. */
export interface UiToolAggregate {
  avg_ms?: number | null;
  calls: number;
  class: ObservationClass;
  /** Finished with `is_error = true`. */
  failed: number;
  name: string;
  /** Finished with `is_error = false`. */
  succeeded?: number;
  total_ms?: number | null;
  /** `tool_call_started` with no matching `tool_call_finished`. */
  unfinished?: number;
}

/** One user shell execution (`!command`) as the reconnect snapshot carries it: the active one plus a bounded recent history. `output_tail` is the bounded end of the combined output (never the full log). */
export interface UiUserShell {
  command: string;
  cwd: string;
  /** Seconds elapsed at snapshot time (running) or total runtime (done). */
  elapsed_secs: number;
  exit_code?: number | null;
  id: UserShellId;
  output_tail?: string;
  /** True when `output_tail` dropped earlier output. */
  output_truncated?: boolean;
  /** `running | success | failed | cancelled`. */
  status: string;
}

/** The verification result. `passed` is `None` while still running. */
export interface UiVerification {
  checks: UiCheck[];
  passed?: boolean | null;
}

/** Identifies one user-originated shell execution (`!command`) — a session-scoped direct host execution. Deliberately NOT a [`ToolCallId`]: a user shell is not an agent tool call and never enters the model conversation. */
export type UserShellId = string;

/** A command from a UI client to the runtime. */
export type ClientCommand =
  /** Submit a user message; the runtime drives a turn in the given session. */
  | { type: 'submit_message'; attachments?: AttachmentRef[]; content: string; session_id: SessionId }
  /** Steer the turn that is already running: the text is injected at the top of the next round instead of waiting for the turn to end. Distinct from queuing a follow-up (which `SubmitMessage` does while busy): a correction like "actually use the other module" is worthless once the work is finished. Ignored when no turn is running — the caller should submit normally in that case. */
  | { type: 'steer_current_turn'; content: string; session_id: SessionId }
  /** Run an explicit goal task. Unlike ordinary chat messages, this enables goal-mode completion (`update_goal`) in the agent loop. */
  | { type: 'run_goal'; content: string; session_id: SessionId }
  /** Import a file as an attachment; the runtime processes and stores it. */
  | { type: 'add_attachment'; path: string; session_id: SessionId }
  /** Import an attachment from immutable base64-encoded bytes already read by a trusted client. This avoids reopening an ambient path after a security-sensitive upload or file-picker validation. */
  | { type: 'add_attachment_data'; data_base64: string; name: string; session_id: SessionId }
  /** Import an image from the system clipboard (spec §38.1). */
  | { type: 'add_clipboard_image'; session_id: SessionId }
  /** Cooperatively cancel the running turn (graceful; resumable). */
  | { type: 'cancel_current_turn'; session_id: SessionId }
  /** Escalate a cancel the user has already requested once. */
  | { type: 'force_cancel_current_turn'; session_id: SessionId }
  /** Resolve a pending permission request . */
  | { type: 'approval_decision'; decision: ApprovalDecision; request_id: ApprovalId }
  /** Answer a pending clarification (spec §35). An empty answer means "skip". */
  | { type: 'answer_clarification'; answer: string; request_id: ClarificationId }
  /** Switch the model used for subsequent turns . */
  | { type: 'select_model'; model: ModelRef; session_id: SessionId }
  /** Switch the execution mode used for subsequent turns . */
  | { type: 'set_permission_profile'; mode: PermissionProfile; session_id: SessionId }
  /** Set product session axes (work profile × collaboration). Wire strings: work_profile = economy|balanced|delivery; collaboration = chat|plan|goal. */
  | { type: 'set_product_axes'; collaboration: string; session_id: SessionId; work_profile: string }
  /** Confirm a collaboration-plan proposal and auto-enter goal mode (K24). */
  | { type: 'confirm_plan_to_goal'; content: string; session_id: SessionId }
  /** List project durable memory (active; optional archived) for TUI/CLI. */
  | { type: 'list_memory'; include_archived?: boolean; session_id: SessionId }
  /** Archive (forget) one active memory id — user-authoritative (no model). */
  | { type: 'forget_memory'; id: string; session_id: SessionId }
  /** Promote one pending candidate to durable memory. User-authoritative: this IS the consent K36 requires, so it is never model-callable. */
  | { type: 'accept_memory'; id: string; session_id: SessionId }
  /** Recompute and push the working-tree diff. */
  | { type: 'request_diff'; session_id: SessionId }
  /** Summarize and compact the conversation history (spec §28, §53). */
  | { type: 'compact_context'; session_id: SessionId }
  /** Start a fresh conversation: drop the session's stored message history so the next turn carries no prior context (a real "new chat", not a screen clear). */
  | { type: 'clear_conversation'; session_id: SessionId }
  /** Ask for the list of stored sessions (spec §52). */
  | { type: 'request_session_list' }
  /** Ask for the session list and route the response only to the requesting session's event stream. */
  | { type: 'request_session_list_for'; requester_session_id: SessionId }
  /** Start a FRESH session and switch the requester's view to it, leaving the current one intact in the session list. This is what `/clear` does. Wiping the current session in place would be destructive and unrecoverable (it takes the checkpoints with it), which is why that shape needed a confirmation; starting a new session loses nothing, so it needs none. */
  | { type: 'new_session_for'; requester_session_id: SessionId }
  /** Open a stored session, loading its transcript into the view. */
  | { type: 'open_session'; session_id: SessionId }
  /** Open a stored session on behalf of another currently displayed session. The switch event is delivered to the requester before the client moves its subscription to the target. */
  | { type: 'open_session_for'; requester_session_id: SessionId; session_id: SessionId }
  /** Delete a stored session. */
  | { type: 'delete_session'; session_id: SessionId }
  /** Delete a stored session and route the refreshed list to the requester. */
  | { type: 'delete_session_for'; requester_session_id: SessionId; session_id: SessionId }
  /** Rename a stored session (overwrite its goal/title text). */
  | { type: 'rename_session'; name: string; session_id: SessionId }
  /** Archive a stored session: it keeps its transcript but leaves the default session list. */
  | { type: 'archive_session'; session_id: SessionId }
  /** Fork a stored session: create a new session with a copy of the transcript, so an alternative direction can be explored without touching the original. */
  | { type: 'fork_session'; session_id: SessionId }
  /** Restore the conversation to a checkpoint (spec §68). */
  | { type: 'restore_checkpoint'; checkpoint_id: CheckpointId; session_id: SessionId }
  /** Side question (`/btw`): single-turn answer using current session context, without tools and without appending to the transcript store. Run an explicit user shell command (`!command`) in the session's repository. USER-ORIGINATED DIRECT EXECUTION: never reaches the model, the agent loop, or the tool registry. `command` is the raw shell string after the `!` prefix. */
  | { type: 'run_user_shell'; command: string; session_id: SessionId }
  /** Cancel exactly one user shell execution. Deliberately separate from `CancelCurrentTurn`: a user shell is not an agent turn, and the id match ensures a stale cancel can never kill a newer execution. */
  | { type: 'cancel_user_shell'; execution_id: UserShellId; session_id: SessionId }
  | { type: 'btw'; question: string; session_id: SessionId }
  /** Read-only observatory query. Does not mutate runtime, tools, or verification. Results arrive as [`crate::RuntimeEvent::ObservabilityLoaded`]. */
  | { type: 'query_observability'; after?: number; before?: number; center_seq?: number | null; session_id: SessionId }
  /** The runtime owner is shutting down; all work should stop. Disconnecting an individual UI client must not issue this command. */
  | { type: 'quit' };

/** An event flowing from the runtime to clients. */
export type RuntimeEvent =
  /** The runtime finished booting and is ready for commands. */
  | { type: 'runtime_ready' }
  /** A session was opened / its snapshot refreshed. */
  | { type: 'session_opened'; session: UiSessionSnapshot }
  /** Session metadata changed (model/mode/branch) without touching the transcript — refresh the header only. */
  | { type: 'session_updated'; session: UiSessionSnapshot }
  /** The runtime needs the user to approve a risky action . */
  | { type: 'approval_requested'; request: UiApprovalRequest }
  /** A pending approval was resolved (by any connected client, or by a timeout/cancel). Clients dismiss the matching prompt so a second client never answers an approval that no longer exists. */
  | { type: 'approval_resolved'; id: ApprovalId }
  /** The agent is asking the user a clarifying question (spec §35). */
  | { type: 'clarification_requested'; request: UiClarificationRequest }
  /** A pending clarification was resolved (by any client, timeout, or cancel). */
  | { type: 'clarification_resolved'; id: ClarificationId }
  /** An imported attachment was processed and stored (spec §39). */
  | { type: 'attachment_added'; attachment: AttachmentRef }
  /** Importing an attachment failed. */
  | { type: 'attachment_processing_failed'; error: string }
  /** A user message was appended to the transcript. */
  | { type: 'user_message_added'; message: UiMessage }
  /** A new assistant message began; deltas will target this id. */
  | { type: 'assistant_message_started'; message_id: MessageId }
  /** A retry attempt began. Remove the prior transient message, if present, and clear its reasoning before applying new deltas. */
  | { type: 'assistant_attempt_reset'; message_id?: MessageId | null }
  /** A chunk of assistant text for an in-flight message. */
  | { type: 'assistant_text_delta'; delta: string; message_id: MessageId }
  /** A chunk of model reasoning/summary, rendered separately from the answer. */
  | { type: 'reasoning_delta'; delta: string }
  /** The assistant message is complete. */
  | { type: 'assistant_message_completed'; message_id: MessageId }
  /** Coarse progress label from the runtime, shown in the status line. */
  | { type: 'agent_activity'; label: string }
  /** Heartbeat while a long command tool runs (runtime observability). Lets a client show "运行 cargo test" with a live elapsed instead of a bare "等待模型". Structured so TUI/Web/logs can consume it uniformly. */
  | { type: 'command_progress'; elapsed_ms: number; label: string }
  /** Project behavior constraints loaded for this turn. Sources are workspace-relative paths; instruction contents never enter UI chrome. */
  | { type: 'project_rules_loaded'; sources: string[] }
  /** A tool call started . */
  | { type: 'tool_call_started'; arguments: string; id: ToolCallId; name: string; parallel?: boolean }
  /** A tool call finished. `preview` is the runtime's truncated output; `duration_ms` is measured client-side. */
  | { type: 'tool_call_completed'; duration_ms: number; id: ToolCallId; ok: boolean; preview: string }
  /** The execution plan was created or a step's status changed (spec §20). */
  | { type: 'plan_updated'; plan: UiPlan }
  /** Verification progress: a check finished or the run concluded (spec §22). */
  | { type: 'verification_updated'; verification: UiVerification }
  /** The working-tree diff was (re)computed (spec §21). */
  | { type: 'diff_updated'; diff: UiDiff }
  /** A conversation checkpoint was created (spec §68). */
  | { type: 'checkpoint_created'; checkpoint: UiCheckpoint }
  /** The list of stored sessions (spec §52). */
  | { type: 'session_list'; sessions: UiSessionSummary[] }
  /** Context package info from an orchestrated run (spec §53). */
  | { type: 'context_updated'; candidate_files: string[]; estimated_tokens: number }
  /** The runtime folded conversation history: `from` transcript messages became `to`. A stable product fact — clients own the wording and the locale; the runtime does not send prose for this. */
  | { type: 'context_compacted'; from: number; to: number }
  /** The context fold threshold climbed one tier on authoritative evidence (adaptive context; production default off). Token budgets, not message counts. `reason` is a stable machine key (e.g. `reread_pressure`). */
  | { type: 'context_expanded'; from_tokens: number; reason: string; to_tokens: number }
  /** A user shell execution (`!command`) started. User-originated direct host execution — not an agent tool call; clients render it as its own block and never feed it to the model conversation. */
  | { type: 'user_shell_started'; command: string; cwd: string; execution_id: UserShellId }
  /** Live output from a running user shell. `stream` is `stdout` or `stderr`. Transient: clients keep a bounded buffer; the runtime does not persist chunks. */
  | { type: 'user_shell_output'; chunk: string; execution_id: UserShellId; stream: string }
  /** A user shell execution ended. `status` is `success | failed | cancelled`; `exit_code` is `None` when the process was killed or never spawned. */
  | { type: 'user_shell_exited'; duration_ms: number; execution_id: UserShellId; exit_code?: number | null; status: string }
  /** Real token usage reported by the model for the latest request. The context gauge tracks how full the window is; `input_tokens` already includes the whole prompt (system + history + tools), so the window in use is `input_tokens + output_tokens`. */
  | { type: 'token_usage'; cached_input_tokens: number; input_tokens: number; output_tokens: number }
  /** An orchestrated run completed; carries the summary report (spec §23). */
  | { type: 'session_completed'; report: UiCompletionReport }
  /** The current turn finished successfully. */
  | { type: 'turn_completed' }
  /** The assistant naturally finished its answer, without claiming that an external task was independently verified as complete. */
  | { type: 'turn_answered' }
  /** The turn stopped at an output limit even after bounded continuation. */
  | { type: 'turn_truncated'; error: string }
  /** The executor stopped cleanly but did not reach a successful terminal state (for example, budget exhaustion or an unresolved goal). */
  | { type: 'turn_incomplete'; reason: string }
  /** The turn finished its work, but leveler could not independently verify it (no verification gate produced passing evidence). Done, not verified — distinct from `TurnIncomplete` (which means the work did not finish). */
  | { type: 'turn_completed_unverified'; reason: string }
  /** The current turn failed. */
  | { type: 'turn_failed'; error: string }
  /** The current turn was cancelled (resumable). */
  | { type: 'turn_cancelled' }
  /** A spawned sub-agent started or finished (multi-agent delegation). One block per agent id, updated in place from running → done. */
  | { type: 'sub_agent_updated'; detail: string; done: boolean; id: string; nickname: string; ok: boolean; role: string }
  /** Live execution state and cumulative model usage for one spawned agent. */
  | { type: 'sub_agent_progress'; active: boolean; cached_input_tokens: number; id: string; input_tokens: number; output_tokens: number }
  /** Live tool/step for one spawned sub-agent (attributed by `id`). Transient; older clients ignore unknown types via [`parse_runtime_event`]. */
  | { type: 'sub_agent_activity'; id: string; is_error: boolean; phase: string; preview: string; tool: string }
  /** A transient notification for the status line. */
  | { type: 'notification'; level: NotificationLevel; message: string }
  /** A background process task was started (`run_command` background=true). */
  | { type: 'background_task_started'; args: string[]; program: string; task_id: string }
  /** A background task finished (exit or kill). */
  | { type: 'background_task_exited'; duration_ms: number; exit_code?: number | null; ok: boolean; task_id: string }
  /** Project memory listing (response to [`crate::ClientCommand::ListMemory`]). */
  | { type: 'memory_list'; active: UiMemoryEntry[]; archived: UiMemoryEntry[]; memory_dir: string; pending?: UiMemoryEntry[] }
  /** Side-question (`/btw`) started; not persisted to session history. */
  | { type: 'btw_started'; question: string }
  /** Side-question answer chunk (often one full answer in MVP). */
  | { type: 'btw_text_delta'; delta: string }
  /** Side-question finished successfully. */
  | { type: 'btw_completed' }
  /** Side-question failed. */
  | { type: 'btw_failed'; error: string }
  /** Coarse turn-progress / closeout signal (additive; protocol minor ≥ 1.2). No free-form paths or tool output — safe to surface in TUI chrome and optional remote summaries. Unknown older clients that reject new variants should skip events via [`crate::event::parse_runtime_event`]. */
  | { type: 'turn_progress'; closeout_deny_rounds: number; closing: boolean; no_progress_streak: number; phase: string }
  /** Result of [`crate::ClientCommand::QueryObservability`]. Read-only projection of durable facts for the current or a historical session. */
  | { type: 'observability_loaded'; observation: UiObservabilityLoaded };
