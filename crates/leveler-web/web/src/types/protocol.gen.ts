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

/** The state of one verification check. This is the check-level fact and not the task-level verdict, so the three ways a check can produce no verdict stay distinct. A check whose program is not installed is not a check that was deliberately skipped, and neither is a check the environment refused: each one is the reason a run ended unverified, and the reader is owed that reason rather than a shrug. */
export type CheckState =
  | 'running' | 'passed' | 'failed'
  /** Deliberately not run. */
  | 'skipped'
  /** No pass/fail observation was produced. `UiCheck::evidence` carries the reason projected by the runtime, such as cancellation or an unavailable dependency. */
  | 'not_run'
  /** The check's program is not on `PATH`, so it could not run at all. */
  | 'tool_missing'
  /** The check ran but the environment refused it (toolchain/MSRV mismatch). */
  | 'environment_unavailable'
  /** The row carried a status this build does not know. It is not a pass, and it is not a skip either — naming it one would invent a reason. */
  | 'unknown';

export interface ChildContribution {
  findings_total: number;
  profile_id?: string | null;
  profile_role?: string | null;
  /** Whether this child held a physically read-only toolset. It used to be a list of semantic capability labels; what a client needs is the structural bound. */
  read_only?: boolean;
  role: string;
}

/** Which bound stopped a child whose stop is [`ChildStop::Budget`]. A mirror of the runtime's typed limit, not a second reason vocabulary: without it a client can only say "budget", which renders a wall-clock timeout and a spent token budget the same. `Duration` is the wall clock — the bound the runtime's child cap enforces. */
export type ChildLimit =
  | 'duration' | 'model_tokens' | 'cost' | 'commands' | 'modified_files'
  /** The child's own round window. */
  | 'round_window'
  /** The absolute round ceiling. */
  | 'round_ceiling';

/** The four-way reading of a settled child's result. "Finished with nothing to flag" and "stopped with nothing to show" are opposite facts; a client renders them from this field, never from the summary text. */
export type ChildOutcome = 'completed_with_findings' | 'completed_no_findings' | 'incomplete_partial' | 'incomplete_no_result';

/** How a settled child's activation ended. */
export type ChildStop = 'completed' | 'incomplete' | 'budget' | 'cancelled' | 'failed' | 'lost';

/** Identifies a pending clarification (ask-user) request. */
export type ClarificationId = string;

/** How one question in a clarification is answered. The kind is explicit rather than inferred from `options` being empty: a single-choice question with no options is unanswerable, while a text question is the only shape whose answer is typed. A client that guessed would silently turn a missing option list into a free-text prompt. */
export type ClarificationQuestionKind =
  /** Exactly one option (or a free-text answer when `allow_other` is set). */
  | 'single'
  /** Zero or more options. */
  | 'multi'
  /** A free-text answer. */
  | 'text';

/** Identifies a client command, used as an idempotency key: a command may be delivered more than once (at-least-once), so the same id must not run the action twice. */
export type CommandId = string;

/** One recorded compaction fold: the estimated transcript size before and after. The runtime records this at the fold boundary, not the TUI. */
export interface CompactionRecord {
  after_tokens: number;
  before_tokens: number;
}

/** What one assembled model request is made of. */
export interface ContextAccounting {
  categories: ContextCategory[];
  /** The fold threshold (`reliable_context`), when known. */
  compact_at_tokens?: number | null;
  /** The model's declared context window (an exact declared fact), when known. */
  context_window_tokens?: number | null;
  /** `context_window - used` when the window is known. */
  free_tokens?: number | null;
  /** Most recent compaction fold, when one has run. */
  last_compaction?: CompactionRecord | null;
  model: ModelRef;
  pressure: ContextPressure;
  token_count_kind: TokenCountKind;
  /** Estimated input tokens of this request — the sum of the top-level categories, by construction. */
  used_tokens: number;
}

/** One mutually exclusive slice of the request. `children` holds the next level of drill-down when the runtime can reliably separate it; a category with no reliable sub-split has none. */
export interface ContextCategory {
  /** Tool invocations counted in this slice (nonzero only for tool leaves and `tool_calls`). */
  calls?: number;
  /** Next drill-down level, when one exists. */
  children?: ContextCategory[];
  /** Presentation label (English, not localized here). */
  label: string;
  /** Stable key (`messages`, `system`, `tool_definitions`, `user`, … or a tool name for a per-tool leaf). The UI may localize by `name`; it must never parse `label`. */
  name: string;
  /** Estimated tokens in this slice. Summing the top level equals [`ContextAccounting::used_tokens`] by construction. */
  tokens: number;
}

/** Deterministic context-pressure level derived from real thresholds — never a model's judgement. */
export type ContextPressure = 'normal' | 'warning' | 'critical';

/** What kind of failure this is, in product terms. Vendor error codes are not part of this vocabulary — `Moonshot invalid_request_error`, `OpenAI invalid_request_error` and `Anthropic invalid_request_error` are all just [`FailureCategory::InvalidRequest`] here. */
export type FailureCategory =
  /** A network/transport failure reaching a provider or service. */
  | 'network'
  /** A timeout elapsed. */
  | 'timeout'
  /** Authentication or authorization failed. */
  | 'authentication'
  /** A rate limit was hit. */
  | 'rate_limit'
  /** The provider itself is unavailable or failed. */
  | 'provider'
  /** The request was malformed or rejected as invalid. */
  | 'invalid_request'
  /** A tool or command failed. */
  | 'tool'
  /** A runtime/infrastructure failure that is not a provider call. */
  | 'runtime'
  /** Permission was refused. */
  | 'permission'
  /** The work was cancelled. */
  | 'cancelled'
  /** Anything not covered above. */
  | 'internal';

/** What the transport could prove about whether the request reached the provider. Mirrors `leveler_model::DeliveryState` in product terms. */
export type FailureDelivery =
  /** Provably never sent. */
  | { kind: 'not_sent' }
  /** Written, but no response was observed. */
  | { kind: 'sent_no_response' }
  /** A complete HTTP status was returned. */
  | { kind: 'responded' }
  /** The response stream began and ended early. `text`/`tool_args` are what it produced before the cut. */
  | { kind: 'stream_interrupted'; text: boolean; tool_args: boolean }
  /** Delivery could not be established. */
  | { kind: 'unknown' };

/** Whether an automatic retry is possible, from the delivery truth. Mirrors `leveler_model::Retryability` in product terms. */
export type FailureRetryability =
  /** The request provably did no provider-side work, or the provider asked to be retried: a bounded automatic retry is safe. */
  | 'safe'
  /** The request may have reached the provider: retrying could duplicate work, so it is never automatic. */
  | 'caution'
  /** Delivery could not be established: never automatic, reported as unknown. */
  | 'unknown'
  /** Retrying the identical request cannot help. */
  | 'never';

/** Which boundary produced the failure. Sources share retry primitives but never share error truth: a provider disconnect, an MCP disconnect, a failed `curl` command and a runtime-IPC drop are different facts. */
export type FailureSource =
  /** The model provider transport. */
  | 'provider'
  /** The runtime itself (persistence, lifecycle, IPC). */
  | 'runtime'
  /** An MCP server transport. */
  | 'mcp'
  /** A tool or command's own network/execution. */
  | 'tool'
  /** Local execution that did not touch the network. */
  | 'local';

/** The mechanical work still running after the assistant has produced its final response but before the runtime publishes the task terminal. This is lifecycle chrome, not a second completion authority: the terminal event remains the only fact that ends the turn. */
export type FinalizationStage =
  /** Wait for work already admitted by the turn to settle. */
  | 'settling_dependencies'
  /** Run the configured checks over the final tree. */
  | 'verification'
  /** Persist verification and other completion evidence. */
  | 'evidence'
  /** Run a completion review explicitly required by the task contract. */
  | 'review'
  /** Resolve the task outcome from the collected facts. */
  | 'resolving_outcome'
  /** Commit and publish the canonical terminal fact. */
  | 'publishing_terminal';

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

/** Why a runtime is being asked to retire. Typed rather than a string because the updater will reuse this exact lifecycle: install an artifact, ask the running runtime to retire, verify the replacement's identity. Only the reason differs. */
export type RestartReason =
  /** The connecting client is a different build than this runtime. */
  | 'build_mismatch'
  /** The connecting client sees a different effective configuration source generation than the running daemon loaded at boot. */
  | 'config_changed'
  /** A newer artifact is installed and waiting to take over. Reserved for the updater; nothing sends it yet. */
  | 'update_ready';

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
  /** The assistant has produced its final response, while the runtime is still settling the task before its one authoritative terminal event. */
  | { type: 'turn_finalizing'; stage: FinalizationStage }
  /** Coarse progress label from the runtime, shown in the status line. */
  | { type: 'agent_activity'; label: string }
  /** Heartbeat while a long command tool runs (runtime observability). Lets a client show "运行 cargo test" with a live elapsed instead of a bare "等待模型". Structured so TUI/Web/logs can consume it uniformly. */
  | { type: 'command_progress'; elapsed_ms: number; label: string }
  /** A model round is about to retry the same request (transient). Belongs in ephemeral status, NOT the transcript: a brief network blip must not spam the conversation. `attempt` is 1-based (the retry about to happen) and `max_attempts` the retry budget; once `attempt == max_attempts` fails the turn is surfaced, not hidden behind an unbounded wait. */
  | { type: 'model_retrying'; attempt: number; delay_ms: number; max_attempts: number }
  /** Project behavior constraints loaded for this turn. Sources are workspace-relative paths; instruction contents never enter UI chrome. */
  | { type: 'project_rules_loaded'; sources: string[] }
  /** A tool call started . */
  | { type: 'tool_call_started'; arguments: string; id: ToolCallId; name: string; parallel?: boolean }
  /** A tool call finished. `preview` is the runtime's truncated output; `duration_ms` is measured client-side. */
  | { type: 'tool_call_completed'; applied_diff?: string | null; duration_ms: number; exit_code?: number | null; id: ToolCallId; ok: boolean; preview: string; stop?: UiCommandStop | null }
  /** Live output from a running command tool call. `stream` is `stdout` or `stderr`; `chunk` is one or more whole, sanitized lines. Transient: clients keep a bounded buffer and the completed preview is the record. */
  | { type: 'tool_call_output'; chunk: string; id: ToolCallId; stream: string }
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
  /** Replay-only. The adaptive-context ladder that climbed the fold threshold was deleted; the variant survives so an old event log still decodes, and nothing emits one. Token budgets, not message counts. */
  | { type: 'context_expanded'; from_tokens: number; reason: string; to_tokens: number }
  /** A user shell execution (`!command`) started. User-originated direct host execution — not an agent tool call; clients render it as its own block and never feed it to the model conversation. */
  | { type: 'user_shell_started'; command: string; cwd: string; execution_id: UserShellId }
  /** Live output from a running user shell. `stream` is `stdout` or `stderr`. Transient: clients keep a bounded buffer; the runtime does not persist chunks. */
  | { type: 'user_shell_output'; chunk: string; execution_id: UserShellId; stream: string }
  /** A user shell execution ended. `status` is `success | failed | cancelled`; `exit_code` is `None` when the process was killed or never spawned. */
  | { type: 'user_shell_exited'; duration_ms: number; execution_id: UserShellId; exit_code?: number | null; status: string }
  /** Real token usage reported by the model for the latest request. The context gauge tracks how full the window is; `input_tokens` already includes the whole prompt (system + history + tools), so the window in use is `input_tokens + output_tokens`. */
  | { type: 'token_usage'; cached_input_tokens: number; input_tokens: number; output_tokens: number }
  /** The accounting of the exact next model request, computed by the kernel before the request is sent. Transient: nothing persists it, and a reconnecting client gets the latest through its live view. */
  | { type: 'context_usage'; accounting: ContextAccounting }
  /** An orchestrated run completed; carries the summary report (spec §23). */
  | { type: 'session_completed'; report: UiCompletionReport }
  /** The current turn finished successfully. */
  | { type: 'turn_completed' }
  /** The work completed and project verification retains its own result, but a separate required completion contract produced warnings. */
  | { type: 'turn_completed_with_warnings'; reason: string }
  /** The assistant naturally finished its answer, without claiming that an external task was independently verified as complete. */
  | { type: 'turn_answered' }
  /** The turn stopped at an output limit even after bounded continuation. */
  | { type: 'turn_truncated'; error: string }
  /** The executor stopped cleanly but did not reach a successful terminal state (for example, budget exhaustion or an unresolved goal). */
  | { type: 'turn_incomplete'; reason: string }
  /** The turn finished its work, but the project's checks did not run or could not produce a verdict. Done, not verified — distinct from `TurnIncomplete` (which means the work did not finish). */
  | { type: 'turn_completed_unverified'; reason: string }
  /** The turn finished its work and the project's own checks then FAILED over the final tree. Done, checks failed — both facts stand; `reason` names the failing checks. */
  | { type: 'turn_completed_checks_failed'; reason: string }
  /** The current turn failed. `error` is the legacy display string, kept for compatibility with older clients (CLI, Web). New presentations must prefer `failure` and must never parse `error` to decide a category, a retry, or a delivery truth. */
  | { type: 'turn_failed'; error: string; failure?: UiFailure | null }
  /** The current turn was cancelled (resumable). */
  | { type: 'turn_cancelled' }
  /** The logical task was explicitly cancelled by the user. Terminal: unlike [`Self::TurnCancelled`] a continuation must not reopen it. */
  | { type: 'task_cancelled' }
  /** A spawned sub-agent started or finished (multi-agent delegation). One block per agent id, updated in place from running → done. */
  | { type: 'sub_agent_updated'; agent?: UiChildAgentIdentity | null; background?: boolean | null; contribution?: ChildContribution | null; detail: string; done: boolean; id: string; limit?: ChildLimit | null; nickname: string; ok: boolean; outcome?: ChildOutcome | null; profile_id?: string | null; profile_role?: string | null; read_only?: boolean; role: string; scope?: string[]; stop?: ChildStop | null; title?: string | null }
  /** A child's lifecycle moved without a start or a terminal: its activation died with a runtime window (`interrupted`) or a new one began under the same id (`running`). Clients update the child they already hold. */
  | { type: 'sub_agent_state_changed'; id: string; state: UiChildState }
  /** Live execution state and cumulative model usage for one spawned agent. */
  | { type: 'sub_agent_progress'; active: boolean; cached_input_tokens: number; id: string; input_tokens: number; output_tokens: number }
  /** Live tool/step for one spawned sub-agent (attributed by `id`). Transient; older clients ignore unknown types via [`parse_runtime_event`]. */
  | { type: 'sub_agent_activity'; id: string; is_error: boolean; phase: string; preview: string; tool: string }
  /** Result of [`crate::ClientCommand::QueryChildContribution`]. Read-only: a snapshot of the ledger, never a mutation. */
  | { type: 'child_contribution_loaded'; detail: UiChildContribution; query_id?: CommandId | null }
  /** A durable goal checkpoint was cut; render it as a Recap history item (long-goal P3). Emitted for `/recap`, milestones, context compaction, and on surfacing an interruption checkpoint — the recap carries its `checkpoint_id`, so an expanded view presents the same persisted facts. */
  | { type: 'goal_recap_created'; recap: UiGoalRecap }
  /** Result of [`crate::ClientCommand::ListUnfinishedGoals`]. Read-only. */
  | { type: 'unfinished_goals_loaded'; goals?: UiUnfinishedGoal[]; query_id?: CommandId | null }
  /** Result of [`crate::ClientCommand::ListAgents`]. */
  | { type: 'agents_loaded'; agents: UiAgentEntry[]; problems?: UiAgentProblem[]; query_id?: CommandId | null }
  /** Result of [`crate::ClientCommand::GetAgent`]. `agent` is `None` and `error` says why when the name does not resolve. */
  | { type: 'agent_loaded'; agent?: UiAgentDetail | null; error?: string | null; name: string; query_id?: CommandId | null }
  /** Result of `CreateAgent` / `UpdateAgent` / `DeleteAgent`. On failure nothing was written and `error` is the reason. */
  | { type: 'agent_mutated'; agent?: UiAgentEntry | null; error?: string | null; name: string; ok: boolean; query_id?: CommandId | null }
  /** A transient notification for the status line. */
  | { type: 'notification'; level: NotificationLevel; message: string }
  /** A background process task was started (`run_command` background=true). */
  | { type: 'background_task_started'; args: string[]; program: string; task_id: string }
  /** A live chunk of a background task's combined stdout/stderr, already sanitized and capped by the runtime. Additive; older clients ignore the unknown type. The final [`Self::BackgroundTaskExited`] log stays authoritative. */
  | { type: 'background_task_output'; chunk: string; task_id: string }
  /** A background task finished (exit or kill). `output` is the task's final retained log, authoritative over the streamed chunks (a lifecycle broadcast lag can drop a chunk). Empty when the runtime produced none. */
  | { type: 'background_task_exited'; duration_ms: number; exit_code?: number | null; ok: boolean; output?: string; stopped?: boolean; task_id: string }
  /** Authoritative replacement of a session's active background projection. Emitted after a lifecycle broadcast lag; history is unaffected. */
  | { type: 'background_tasks_reconciled'; tasks: UiActiveBackgroundTask[] }
  /** Project memory listing (response to [`crate::ClientCommand::ListMemory`]). */
  | { type: 'memory_list'; active: UiMemoryEntry[]; archived: UiMemoryEntry[]; memory_dir: string; pending?: UiMemoryCandidate[] }
  /** The runtime recalled durable project memory into this turn's model context. Emitted ONLY when at least one memory was selected — a search that found nothing is not a recall. Ids/count only, never bodies. */
  | { type: 'memory_recalled'; count: number; ids: string[] }
  /** A durable memory lifecycle change (created / superseded / expired / merged). `operation` is an opaque stable key; `title` is a bounded summary, never the body. */
  | { type: 'memory_changed'; authority?: string | null; id: string; operation: string; title: string }
  /** Side-question (`/btw`) started; not persisted to session history. */
  | { type: 'btw_started'; question: string }
  /** Side-question answer chunk (often one full answer in MVP). */
  | { type: 'btw_text_delta'; delta: string }
  /** Side-question finished successfully. */
  | { type: 'btw_completed' }
  /** Side-question stopped by the user before it finished. Distinct from [`Self::BtwFailed`]: the answer was interrupted, not unsuccessful, and the partial answer (if any) is what the side thread keeps. */
  | { type: 'btw_cancelled' }
  /** Side-question failed. */
  | { type: 'btw_failed'; error: string }
  /** Coarse turn-progress / closeout signal (additive; protocol minor ≥ 1.2). No free-form paths or tool output — safe to surface in TUI chrome and optional remote summaries. Unknown older clients that reject new variants should skip events via [`crate::event::parse_runtime_event`]. */
  | { type: 'turn_progress'; closing: boolean; no_progress_streak: number; phase: string }
  /** Result of [`crate::ClientCommand::QueryContext`]. `accounting` is `None` when no model request has been assembled yet for the session (a fresh session before its first turn). */
  | { type: 'context_loaded'; accounting?: ContextAccounting | null; query_id?: CommandId | null }
  /** Result of [`crate::ClientCommand::QueryObservability`]. Read-only projection of durable facts for the current or a historical session. Echoes the command's `query_id` when the peer sent one. Absent on protocol 1.5 peers — a current client must not treat that as ownership. */
  | { type: 'observability_loaded'; observation: UiObservabilityLoaded; query_id?: CommandId | null }
  /** Result of [`crate::ClientCommand::QuerySessionHistory`]: the latest turns in order. `omitted_turns` older turns were left out to bound the response; they are still in the session. */
  | { type: 'session_history_loaded'; entries: UiHistoryEntry[]; omitted_turns?: number; query_id?: CommandId | null; session_id: SessionId };

/** Identifies a single agent session (one user goal end to end). */
export type SessionId = string;

/** How the token figures in a snapshot were produced. */
export type TokenCountKind =
  /** Every figure came from a real tokenizer. */
  | 'exact'
  /** Every figure came from the byte/char estimator. */
  | 'estimated'
  /** The total is provider-reported but the breakdown is estimated. */
  | 'mixed';

/** Identifies a tool call. Must be stable across streaming reassembly. */
export type ToolCallId = string;

/** One background process that is still lifecycle-active in the runtime. This is a reconnect projection, not history. Terminal tasks never appear here even though the execution registry may retain their records for `get`/`wait`. */
export interface UiActiveBackgroundTask {
  args: string[];
  /** Runtime-observed age at snapshot time. */
  elapsed_ms: number;
  program: string;
  task_id: string;
}

/** A tool invocation that was still running when a client took its snapshot. */
export interface UiActiveToolCall {
  arguments: string;
  /** How long the call had been running when the snapshot was taken, by the runtime's clock, so a reconnecting client does not restart it at zero. */
  elapsed_ms?: number;
  id: ToolCallId;
  name: string;
  /** The bounded end of the command's live output so far. */
  output_tail?: string;
  /** True when `output_tail` dropped earlier output. */
  output_truncated?: boolean;
}

/** The runtime capability class an agent runs under. */
export type UiAgentCapability = 'read_only' | 'writer' | 'scoped_writer';

/** One agent with its full definition, for an editor. */
export interface UiAgentDetail {
  entry: UiAgentEntry;
  /** `None` for structural built-ins and invalid definitions. */
  instructions?: string | null;
}

/** A definition a client asks the runtime to write. */
export interface UiAgentDraft {
  capability: UiAgentCapability;
  description: string;
  instructions: string;
  max_duration_secs?: number | null;
  max_rounds?: number | null;
  model?: string | null;
  name: string;
  reasoning_effort?: string | null;
  skills?: string[];
  tools?: string[] | null;
  write_roots?: string[];
}

/** One resolvable agent name, as the registry resolves it. */
export interface UiAgentEntry {
  capability?: UiAgentCapability | null;
  /** The fields below are `None`/empty for an invalid definition. */
  description?: string | null;
  fingerprint?: string | null;
  /** Launched by the harness only; a model cannot spawn it. */
  harness_only?: boolean;
  /** The definition's directory; `None` for built-ins. */
  location?: string | null;
  max_duration_secs?: number | null;
  max_rounds?: number | null;
  model?: string | null;
  name: string;
  /** Why it is unavailable or invalid. */
  reason?: string | null;
  reasoning_effort?: string | null;
  shadowed?: UiShadowedAgent[];
  skills?: string[];
  source: UiAgentSource;
  status: UiAgentStatus;
  /** One of the runtime's structural roles (default, explorer, worker, reviewer): not editable, and not overridable. */
  structural?: boolean;
  /** `None`: the capability's full toolset. */
  tools?: string[] | null;
  write_roots?: string[];
}

/** Durable sub-agent / reviewer observation. */
export interface UiAgentObservation {
  /** The declarative agent the child was spawned from (`security-reviewer`), when it was; `role` is then only its capability class. */
  agent?: string | null;
  id: string;
  nickname: string;
  role: string;
  status: string;
  summary?: string;
}

/** Something under an agents directory that is not an agent at all. */
export interface UiAgentProblem {
  error: string;
  location: string;
  source: UiAgentSource;
}

/** Where a client may write a definition. */
export type UiAgentScope = 'project' | 'user';

/** Where a definition came from. */
export type UiAgentSource = 'project' | 'user' | 'builtin';

/** Whether an entry can be spawned here. */
export type UiAgentStatus =
  /** Valid, and everything it names exists on this machine. */
  | 'available'
  /** Valid, but something it names is missing here (model, skill, effort). */
  | 'unavailable'
  /** The definition itself is broken; it cannot be spawned anywhere. */
  | 'invalid';

/** A pending permission request, projected for display. */
export interface UiApprovalRequest {
  /** Whether "always allow" would persist a standing permission rule for this action. When `false` (consent tools such as `save_agent` or `remember`, actions with no safe rule shape, or a request recorded before this field) the runtime could only honour it for this turn, so a client must not offer it. */
  always_persists?: boolean;
  /** The tool call this request is holding, when the runtime knows it. A UI needs it to tell "announced, waiting for you" apart from "running": the call has an event on screen already, and without an id the only way to find its row would be to guess at the latest running call. `None` for a request that is not about one specific call (a standing `request_permissions`), and for a session recorded before this field. */
  call_id?: string | null;
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

/** One delegated child, projected from the durable record — what a client that reconnects, or opens a session, renders without having seen a single live event. */
export interface UiChildAgent {
  /** The declarative agent it was spawned from, when it was. `None` for a built-in role spawn and for children recorded before agents existed. */
  agent?: UiChildAgentIdentity | null;
  /** Whether its parent continued while it ran. */
  background?: boolean;
  /** `None` when no call carried a price — unknown, not zero. */
  cost_usd_micros?: number | null;
  id: string;
  /** Model usage recorded under this child's id. */
  input_tokens?: number;
  /** Which bound fired when `stop` is [`ChildStop::Budget`]. `None` for every other stop and for terminals recorded before it was carried. */
  limit?: ChildLimit | null;
  nickname: string;
  /** Whether it reached the end of its task, once settled. Carried beside `outcome` because rows settled before the outcome was typed have only this bit. */
  ok?: boolean;
  outcome?: ChildOutcome | null;
  output_tokens?: number;
  profile_id?: string | null;
  /** What it was asked to do. */
  purpose: string;
  read_only?: boolean;
  /** How many times it was continued after an interruption. */
  resumes?: number;
  role: string;
  /** Exclusive write scope fixed at spawn (empty when late-bound or read-only). */
  scope?: string[];
  state: UiChildState;
  stop?: ChildStop | null;
  /** The recorded settlement summary, once settled. */
  summary?: string | null;
  /** The child's short task title, fixed at spawn: its identity, distinct from the full instructions in `purpose`. `None` on children recorded before titles existed; a presentation fallback may project `purpose`. */
  title?: string | null;
}

/** The definition a running or settled child was spawned from, as resolved at spawn. Deleting or editing the definition does not change it. */
export interface UiChildAgentIdentity {
  capability: string;
  fingerprint: string;
  model?: string | null;
  /** The agent's name, e.g. `security-reviewer`. */
  name: string;
  reasoning_effort?: string | null;
  skills?: string[];
  /** `project`, `user` or `builtin`. */
  source: string;
}

/** Everything the inspector shows for one child. */
export interface UiChildContribution {
  child_id: string;
  /** Findings this child produced, in ledger order. */
  findings?: UiFinding[];
  /** Whether a ledger snapshot was found at all. `false` means the question could not be answered — no ledger, or the child predates finding adoption. It does NOT mean the child found nothing, and the inspector must not render it that way. */
  measured: boolean;
  profile_id?: string | null;
  /** Whether this child held a physically read-only toolset. */
  read_only?: boolean;
  role: string;
}

/** What one child contributed, as counts plus its capability contract. A flat mirror of the runtime's projection rather than the runtime type itself: this crate is the stable wire, so an internal refactor of the ledger must not change what clients parse. `findings_total` is a count, not a score: it says how much this child reported, never whether any of it mattered. What the parent did about it is in the transcript, where the parent said it. Where a delegated child's lifecycle stands, as the runtime records it. */
export type UiChildState =
  /** An activation is live. */
  | 'running'
  /** Its activation died with a runtime window; the next turn of the session continues it or settles it as lost. */
  | 'interrupted'
  /** It has its one terminal. */
  | 'settled';

/** One question of a clarification interaction (spec §35). A clarification is a set of questions the user answers in one sitting; the client shows them as tabs so only one is on screen at a time. `header` is the short tab label, `question` the full prompt. */
export interface UiClarificationQuestion {
  /** Offer a trailing free-text entry ("其他…") next to the options. */
  allow_other?: boolean;
  /** Short label for the question's tab. Empty means "derive one from `question`" — a display concern the client owns. */
  header?: string;
  kind?: ClarificationQuestionKind;
  /** Maximum number of picks for a `multi` question (`None` = the option count). */
  max_choices?: number | null;
  /** Minimum number of picks for a `multi` question (0 = none required). */
  min_choices?: number;
  /** Candidate answers for `single` / `multi`. Empty for `text`. */
  options?: string[];
  question: string;
}

/** A mid-task clarification the agent needs answered (spec §35). */
export interface UiClarificationRequest {
  id: ClarificationId;
  /** Candidate answers, when the model offered a choice. */
  options: string[];
  /** The interaction's headline, and the whole prompt for a request that predates `questions`. */
  question: string;
  /** The questions to answer in one interaction. Empty for a legacy single-question request: clients then render `question`/`options` as one question, so an older runtime keeps working against a newer client and vice versa. */
  questions?: UiClarificationQuestion[];
}

/** How a stopped command call ended, as the runtime established it. */
export type UiCommandStop =
  /** The whole process tree was confirmed gone. */
  | 'confirmed'
  /** The tree was signalled, but its termination could not be confirmed. */
  | 'unconfirmed';

/** The final completion report (spec §23). */
export interface UiCompletionReport {
  added: number;
  checks_passed: number;
  checks_total: number;
  files_changed: number;
  removed: number;
  /** Whether the run completed and every gating check passed. Kept for existing clients; `verification` carries the full status. */
  success: boolean;
  /** The project's own checks over the final tree. Absent on reports written before the status/verification split (reads as `not_run`). */
  verification?: UiVerificationStatus;
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

/** A structured product failure attached to a terminal turn failure. `detail` is the raw diagnostic and belongs on disclosure only; every other field is machine semantics suitable for choosing an icon, a title, and an action without reading the message. */
export interface UiFailure {
  category: FailureCategory;
  delivery: FailureDelivery;
  /** The raw technical detail. Disclosure only — never the primary line. */
  detail: string;
  /** The model id the failing request was addressed to, when known. */
  model?: string | null;
  /** The provider id, when the failure came from a provider call. */
  provider?: string | null;
  /** The provider's own error code, when it reported one. */
  provider_code?: string | null;
  /** The provider's correlation id for the failing request, when the response exposed one. The single most useful field for a support report against a vendor. */
  request_id?: string | null;
  /** Automatic retries the runtime spent before this became terminal. Absent when the failure never went through a retry loop. */
  retries?: number | null;
  retryability: FailureRetryability;
  source: FailureSource;
  /** HTTP status, when the failure had one. */
  status?: number | null;
  /** A short, provider-agnostic one-line product explanation for the primary line (e.g. "模型服务拒绝了当前请求。"). Clients may override it with their own localization keyed on `category`. */
  summary: string;
}

/** One finding, as the inspector shows it. A projection of `leveler_lifecycle::FindingRecord`, not the record itself: this crate is the stable wire, and the ledger must stay free to change. */
export interface UiFinding {
  file?: string | null;
  /** Parent-ledger id (`f-1`). Stable enough for a user to refer to. */
  id: string;
  /** `relevant_file`, `risk`, `correctness`, … */
  kind: string;
  summary: string;
  symbol?: string | null;
}

/** One durable goal checkpoint, projected for history presentation (long-goal P3). Every Recap a client renders maps to exactly one persisted checkpoint (`checkpoint_id`) — the client presents these fields, it never rebuilds its own summary, and it never parses `display_summary` to reconstruct facts. Truth rules ride the shape: `findings_total == None` means the ledger was not readable when the checkpoint was cut — UNKNOWN, which a client must never render as zero. `verification` is `"unmeasured"` when nothing was proven — never a pass. */
export interface UiGoalRecap {
  /** The persisted `GoalCheckpointId` this Recap presents. */
  checkpoint_id: string;
  completed_milestones?: string[];
  /** RFC3339. When the checkpoint was cut. */
  created_at: string;
  /** The 1–2 line presentation. Runtime-rendered: the semantic summary when one exists, otherwise the deterministic structured fallback. */
  display_summary: string;
  /** `None` = UNKNOWN (ledger unreadable) — never render as 0. */
  findings_total?: number | null;
  goal_id: string;
  known_limitations?: string[];
  next_action?: string | null;
  phase?: string | null;
  /** Plan progress; both `None` when no plan was recorded (absent, not 0/0). */
  plan_completed?: number | null;
  plan_total?: number | null;
  /** `manual` | `milestone` | `context_compaction` | `interrupted`. */
  reason: string;
  /** Transcript position the checkpoint represents (messages `[0..n)`), so a reopened session can interleave the Recap where it happened. `None` = unknown; append after existing history. */
  transcript_ordinal?: number | null;
  unresolved_work?: string[];
  /** `passed` | `failed` | `unmeasured`. */
  verification: string;
  /** Evidence for a pass, or the failure detail. Absent when unmeasured. */
  verification_detail?: string | null;
}

/** One fact of a past turn, as the live stream carried it. */
export interface UiHistoryEntry {
  event: RuntimeEvent;
  /** Milliseconds from the start of the turn this entry belongs to, taken from the durable record times — a replay has no live clock. */
  turn_elapsed_ms: number;
  /** The first entry of a turn. */
  turn_start?: boolean;
}

/** What one lane of a session spent. `lane` is `main`, `children` or `total`. */
export interface UiLaneAccounting {
  cached_input_tokens?: number | null;
  cost_usd_micros?: number | null;
  input_tokens: number;
  lane: string;
  output_tokens: number;
  requests: number;
}

/** A candidate awaiting consent, with enough to decide on. Deliberately NOT [`UiMemoryEntry`]: approving something shown only as a title is not informed consent, so the body, kind and source ride along. */
export interface UiMemoryCandidate {
  /** The text that would be stored. Already length-bounded by the domain. */
  body: string;
  id: string;
  /** Free-form label as stored (`preference`, `package_manager`, …). */
  kind: string;
  /** Who proposed it (`user_explicit`, `system_propose`, …). */
  source: string;
  title: string;
}

/** Compact durable-memory row for TUI list surfaces. */
export interface UiMemoryEntry {
  id: string;
  /** What this memory is, which decides how it reaches the model. Additive: absent on older runtimes. */
  kind?: UiMemoryKind | null;
  /** Held back from every automatic path because it looks like a credential. Kept in the listing so the user can find and archive it. */
  sensitive?: boolean;
  title: string;
}

/** What a durable memory IS. Strongly typed so no client invents its own strings for a field the domain has to interpret. */
export type UiMemoryKind =
  /** Injected into every turn while active. */
  | 'preference'
  /** Reachable by query recall and the catalog, never auto-injected. */
  | 'decision'
  /** Same reach as a decision. */
  | 'note';

/** A rendered message in the transcript. */
export interface UiMessage {
  id: MessageId;
  /** How many images this message carried. A client renders them its own way — the text never holds a file path, and a message that was only a picture would otherwise read as an empty one. Additive: 0 on old runtimes and on every message that carried none. */
  images?: number;
  /** What kind of message this is when its role alone would mislead. `None` is an ordinary message of its role. */
  kind?: UiMessageKind | null;
  /** Persisted transcript ordinal (append order in the message log), when known. Lets a client interleave durable goal recaps at the position their checkpoint represents. Additive: absent on live-stream messages and old runtimes. */
  ordinal?: number | null;
  role: UiRole;
  text: string;
}

/** A message whose role does not say who wrote it. */
export type UiMessageKind =
  /** Written by the runtime into the model's context as a user-role turn — a child's settlement, a lost or resumed delegation — not typed by the user. Render it as a runtime notice, never as user input. */
  | 'runtime_notice';

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
  review_stages: string[];
  workspace_snapshots: number;
}

/** One durable model-request row (no prompt/body). */
export interface UiRequestObservation {
  /** The sub-agent that made the call; absent for the root session's own. */
  agent_id?: string | null;
  cached_input_tokens?: number | null;
  cost_usd_micros?: number | null;
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
  /** Prompt tokens the provider served from cache — a SUBSET of `input_tokens`. `None` when no request recorded the figure at all; that is an absence of measurement, and summing it as zero would report a cache miss nobody observed. */
  cached_input_tokens?: number | null;
  collaboration: string;
  compact_count: number;
  /** Summed over the requests that carry a price. `None` when none does. */
  cost_usd_micros?: number | null;
  created_at: string;
  /** Wall clock from the session's first to its latest durable timestamp. `None` when either end is unparseable. */
  duration_ms?: number | null;
  goal: string;
  input_tokens: number;
  /** Spend split by who did it: the root session, its children, and the total. Present whenever there is a request to attribute. */
  lanes?: UiLaneAccounting[];
  last_latency_ms?: number | null;
  last_sequence?: number | null;
  model: string;
  output_tokens: number;
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
  /** The latest verdict the runtime actually recorded: `passed`, `failed`, `not_run` (nothing ever started), or `unavailable` (started and never reached a verdict). A count of runs is not a verdict. */
  verification: string;
  verification_runs: number;
  work_profile: string;
}

/** Everything a client needs to render a session's header and transcript. */
export interface UiSessionSnapshot {
  /** Registry-backed background processes still active for this session. Additive/defaulted so older runtimes decode as no known live process. */
  active_background_tasks?: UiActiveBackgroundTask[];
  /** Live render state needed to reconnect while a long turn is still running. All fields are additive/defaulted for protocol compatibility. */
  active_tools?: UiActiveToolCall[];
  /** Models the user can switch to (for the model picker, ). */
  available_models?: ModelRef[];
  /** VCS branch, if the repository is a git repo. */
  branch?: string | null;
  checkpoints?: UiCheckpoint[];
  /** Every delegated child of this session, oldest first, from the durable record. Additive: absent on old runtimes. */
  children?: UiChildAgent[];
  /** Product collaboration axis (`chat | plan | goal`). Same contract as `work_profile` — the runtime routes submits (goal) and restricts tools (plan) from this value, so clients must not invent it. */
  collaboration?: string | null;
  completion_report?: UiCompletionReport | null;
  diff?: UiDiff | null;
  /** Typed in-flight terminalization stage. Present only while the runtime still owns an active turn after the final assistant message. */
  finalization_stage?: FinalizationStage | null;
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
  /** Durable goal recaps for this session's goals (long-goal P3), oldest first. Each maps to one persisted GoalCheckpoint; a reopened client interleaves them into history by `transcript_ordinal`. Additive. */
  recaps?: UiGoalRecap[];
  repository: string;
  /** Persisted status string (e.g. "running", "completed"). */
  status: string;
  /** User shell executions: the active one (if any) plus a bounded recent history, newest last. Additive/defaulted like the rest of this block. */
  user_shells?: UiUserShell[];
  verification?: UiVerification | null;
  /** Whether the current model accepts image input (spec §42). */
  vision?: boolean;
  /** Product work-profile axis (`economy | balanced`; legacy `delivery` reads as `balanced`). The source of truth is the session record (`SetProductAxes`); carried here so a reconnecting client shows the axis the runtime will actually use instead of a stale local guess. Absent on old runtimes. */
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

/** A lower-precedence definition the active one hides. */
export interface UiShadowedAgent {
  location?: string | null;
  source: UiAgentSource;
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

/** One goal that still owes work. */
export interface UiUnfinishedGoal {
  /** A turn is still running for it — it is being driven right now, and is therefore not unfinished work. */
  driving: boolean;
  goal_id: string;
  /** What the user asked, verbatim. */
  objective: string;
  /** RFC3339. When the goal was opened. */
  opened_at: string;
  /** Whether this runtime may act on it. `false` means another runtime holds the task; the goal is still listed, because omitting work that exists is worse than naming work nobody here can act on. */
  ours: boolean;
  /** The conversation it ran in, so the user can go read it. */
  session_id: string;
  /** Work windows it consumed before stopping. */
  windows_run: number;
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

/** The verification result. `passed` is `None` while a check is still running, and it is also `None` when nothing was proven. It is never `Some(true)` for a run that was not verified — "not verified" and "failed" are different facts, and clients render them differently (`incomplete` versus `failed`). */
export interface UiVerification {
  checks: UiCheck[];
  passed?: boolean | null;
}

/** What the project's own checks reported over the final tree. Orthogonal to whether the run completed: `Passed` means the configured commands exited 0, never that the user's request was satisfied. */
export type UiVerificationStatus = 'passed' | 'failed' | 'not_run' | 'unavailable';

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
  /** Run the Develop workflow on an explicit goal: `Analyze → Coding → Verify → Review`. Distinct from [`Self::RunGoal`] because it is a different product promise, not a different phrasing of the same one: the user is asking for the change to be investigated before it is written and read back after it is verified. An ordinary message never becomes one of these. */
  | { type: 'run_develop'; content: string; session_id: SessionId }
  /** Continue the session's interrupted logical task through the runtime's existing resume path, instead of starting a fresh turn. The client sends this when the user expressed continuation intent (see [`crate::parse_continuation`]); the runtime remains the authority on whether a resumable task actually exists. When none does, the runtime falls back to the ordinary submit path, so a `ResumeTask` is never lost — it is either a resume or a normal message. `content` is the user's own text (for example `继续，但是先不要跑测试`): the runtime parses the amendment off it, preserving the original objective and adding the instruction, never replacing it. */
  | { type: 'resume_task'; content: string; session_id: SessionId }
  /** Import a file as an attachment; the runtime processes and stores it. `name` overrides the display name that would otherwise come from the file name. A terminal that answers Cmd+V on an image by writing a scratch file and pasting its path knows the file name is its own bookkeeping (`clipboard-2026-09-16-215212-CE7C9522.png`) and not something the user chose — only the client that made the gesture knows that, so only it can say so. */
  | { type: 'add_attachment'; name?: string | null; path: string; session_id: SessionId }
  /** Import an attachment from immutable base64-encoded bytes already read by a trusted client. This avoids reopening an ambient path after a security-sensitive upload or file-picker validation. */
  | { type: 'add_attachment_data'; data_base64: string; name: string; session_id: SessionId }
  /** Import an image from the system clipboard (spec §38.1). */
  | { type: 'add_clipboard_image'; session_id: SessionId }
  /** Retire this runtime once its current work has settled. One mechanism covers both cases the caller cares about: an idle runtime drains instantly and exits, a busy one stops taking new work and exits when the work it already owns is done. The caller never polls for idleness and never kills anything — the runtime owns the drain, because only it knows what "still working" means. */
  | { type: 'shutdown_when_idle'; reason: RestartReason }
  /** Cooperatively cancel the running turn (graceful; resumable). */
  | { type: 'cancel_current_turn'; session_id: SessionId }
  /** Escalate a cancel the user has already requested once. */
  | { type: 'force_cancel_current_turn'; session_id: SessionId }
  /** Cancel the logical task, not just the running turn. Distinct from [`Self::CancelCurrentTurn`]: that interrupts the current work window and leaves the task resumable, while this is the user saying the task itself is over. The runtime commits a terminal `cancelled` outcome, settles the goal, and refuses a later continuation — a `继续` must not silently reopen it. */
  | { type: 'cancel_task'; session_id: SessionId }
  /** Cancel ONE running delegated child of the session's turn. The child settles as cancelled; its parent turn keeps running. */
  | { type: 'cancel_child'; child_id: string; session_id: SessionId }
  /** Stop ONE running tool call of the session's turn (for a command, its whole process tree). The call settles with its own stop outcome; the turn keeps running. Rejected when no such call is executing. */
  | { type: 'cancel_tool_call'; call_id: ToolCallId; session_id: SessionId }
  /** Resolve a pending permission request . */
  | { type: 'approval_decision'; decision: ApprovalDecision; request_id: ApprovalId }
  /** Answer a pending clarification (spec §35). An empty answer means "skip". */
  | { type: 'answer_clarification'; answer: string; request_id: ClarificationId }
  /** Switch the model used for subsequent turns . */
  | { type: 'select_model'; model: ModelRef; session_id: SessionId }
  /** The user's explicit "use this model from now on": switch the active session's model AND persist it as the user's default model for future sessions. Distinct from [`Self::SelectModel`] on purpose. Only this command carries the authority to rewrite the user's persisted configuration, because only an explicit user action does. Runtime fallbacks, provider failover, retries, temporary CLI/session overrides and internal routing MUST use [`Self::SelectModel`] (session-scoped) and never touch the default. */
  | { type: 'set_default_model'; model: ModelRef; session_id: SessionId }
  /** Switch the execution mode used for subsequent turns . */
  | { type: 'set_permission_profile'; mode: PermissionProfile; session_id: SessionId }
  /** Set product session axes (work profile × collaboration). Wire strings: work_profile = economy|balanced (legacy `delivery` reads as `balanced`); collaboration = chat|plan|goal. */
  | { type: 'set_product_axes'; collaboration: string; session_id: SessionId; work_profile: string }
  /** Confirm a collaboration-plan proposal and auto-enter goal mode (K24). */
  | { type: 'confirm_plan_to_goal'; content: string; session_id: SessionId }
  /** List project durable memory (active; optional archived) for TUI/CLI. */
  | { type: 'list_memory'; include_archived?: boolean; session_id: SessionId }
  /** Archive (forget) one active memory id — user-authoritative (no model). */
  | { type: 'forget_memory'; id: string; session_id: SessionId }
  /** Promote one pending candidate to durable memory. User-authoritative: this IS the consent K36 requires, so it is never model-callable. */
  | { type: 'accept_memory'; id: string; session_id: SessionId }
  /** Decline one pending candidate and suppress the same signal from being re-proposed. Distinct from [`Self::ForgetMemory`], which archives an ACTIVE entry: sending a pending id to forget did nothing at all, which is what the Web "忽略" button was doing. */
  | { type: 'reject_memory'; id: string; session_id: SessionId }
  /** Create a durable memory directly. The user's own command IS the authorization, so this never reaches the model and never becomes a pending candidate — it is the deterministic counterpart to the natural-language path, which can only propose. */
  | { type: 'remember_memory'; body: string; kind?: UiMemoryKind | null; session_id: SessionId }
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
  /** Stop one background task the runtime is still running. The registry signals the process tree through its own lifecycle; the task settles as killed. Deliberately separate from `CancelCurrentTurn` and `CancelUserShell`: a background task outlives the turn that started it, and stopping it must not stop the turn. */
  | { type: 'cancel_background_task'; session_id: SessionId; task_id: string }
  | { type: 'btw'; question: string; session_id: SessionId }
  /** Stop the in-flight `/btw` side answer for a session. Deliberately separate from `CancelCurrentTurn`: a side thread's answer has its own lifecycle, and stopping it must never stop the main turn. */
  | { type: 'cancel_btw'; session_id: SessionId }
  /** Read-only observatory query. Does not mutate runtime, tools, or verification. Results arrive as [`crate::RuntimeEvent::ObservabilityLoaded`]. */
  | { type: 'query_observability'; after?: number; before?: number; center_seq?: number | null; query_id?: CommandId | null; session_id: SessionId }
  /** Read-only context-accounting query. Does not mutate the runtime or the conversation. Answers with [`crate::RuntimeEvent::ContextLoaded`]. */
  | { type: 'query_context'; query_id?: CommandId | null; session_id: SessionId }
  /** Load one child's findings for the Contribution Inspector. A query, not a subscription: findings live in the ledger, and pushing them through the event stream would duplicate the record and re-grow the payloads the event pipeline was trimmed of. The client asks when the user opens a detail view. */
  | { type: 'query_child_contribution'; child_id: string; query_id?: CommandId | null; session_id: SessionId }
  /** Cut a durable goal checkpoint for the session's current goal and surface it as a Recap (long-goal P3, `/recap`). This is NOT "summarize the visible transcript": the runtime projects authoritative facts (event log, evidence ledger, goal row) into a persisted GoalCheckpoint and answers with [`crate::RuntimeEvent::GoalRecapCreated`]. Idempotent at the same event boundary — a transport retry returns the same checkpoint. */
  | { type: 'recap'; session_id: SessionId }
  /** List goals that still owe work (long-goal P2). Read-only, and there is deliberately no companion command that continues one: resume is a policy this runtime has not decided, and a protocol that can only report is a protocol that cannot accidentally restart somebody's half-finished mutation. */
  | { type: 'list_unfinished_goals'; query_id?: CommandId | null; session_id: SessionId }
  /** The session's past turns as clients saw them live: its durable event log projected through the same client projection, with the user's messages in place. Answered by [`crate::RuntimeEvent::SessionHistoryLoaded`]. */
  | { type: 'query_session_history'; query_id?: CommandId | null; session_id: SessionId }
  /** List the agent definitions the session's project resolves. Answered by [`crate::RuntimeEvent::AgentsLoaded`]. */
  | { type: 'list_agents'; query_id?: CommandId | null; session_id: SessionId }
  /** One agent with its full definition. Answered by [`crate::RuntimeEvent::AgentLoaded`]. */
  | { type: 'get_agent'; name: string; query_id?: CommandId | null; session_id: SessionId }
  /** Write a new agent definition. The user's own command is the authorization; the runtime validates and writes atomically, and answers with [`crate::RuntimeEvent::AgentMutated`]. */
  | { type: 'create_agent'; draft: UiAgentDraft; query_id?: CommandId | null; scope: UiAgentScope; session_id: SessionId }
  /** Replace an existing agent definition in `scope`. */
  | { type: 'update_agent'; draft: UiAgentDraft; query_id?: CommandId | null; scope: UiAgentScope; session_id: SessionId }
  /** Delete an agent definition from `scope`. Running children keep theirs. */
  | { type: 'delete_agent'; name: string; query_id?: CommandId | null; scope: UiAgentScope; session_id: SessionId }
  /** The runtime owner is shutting down; all work should stop. Disconnecting an individual UI client must not issue this command. */
  | { type: 'quit' };
