//! JSONL desktop adapter; runtime host owns daemon lifetime and session truth.
use leveler_client_protocol::mcp::{McpRequest, McpServerSpec, McpTransport};
use leveler_client_protocol::schedule::{
    ScheduleActionWire, ScheduleDraft, SchedulePatch, ScheduleRequest, ScheduleTriggerWire,
};
use leveler_client_protocol::{
    ClientCommand, CommandEnvelope, InteractiveRuntimeClient, SessionId,
};
use leveler_local_transport::{
    CreateSessionRequest, CreateWorkspaceSelection, LocalRuntimeService, LocalSocketRuntimeClient,
};
use leveler_project::Layout;
use leveler_runtime_host::{
    DaemonReviver, DetachedRuntimeLaunch, HandoffAction, HandoffEvent, HandoffUi,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
    task::JoinHandle,
};

#[derive(Deserialize)]
struct Request {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

struct DesktopHandoff(mpsc::Sender<Value>);
impl HandoffUi for DesktopHandoff {
    fn emit(&self, event: HandoffEvent) {
        // The typed event travels as itself. The Desktop is a shell that cannot
        // ask a human here, so the only thing it can do with a handover is show
        // it — and a `Debug` rendering is prose that silently drifts the moment
        // a variant is renamed. A client branches on `kind`.
        let _ = self.0.try_send(json!({"event": "handoff", "data": event}));
    }
    fn input(&self) -> Option<mpsc::UnboundedReceiver<HandoffAction>> {
        None
    }
}

fn validate_command(session: &SessionId, command: &ClientCommand) -> anyhow::Result<()> {
    match command {
        ClientCommand::SubmitMessage { attachments, .. }
            if attachments.len() <= 16 && attachments.iter().all(valid_desktop_image) => {}
        // Main constructs this only from bounded, native-picker-selected
        // immutable bytes. No ambient path upload is accepted by the adapter.
        ClientCommand::AddAttachmentData {
            name, data_base64, ..
        } if !name.is_empty()
            && name.len() <= 1024
            && data_base64.len() <= (20 * 1024 * 1024_usize).div_ceil(3) * 4 => {}
        ClientCommand::ListAgents { query_id, .. }
        | ClientCommand::QueryContext { query_id, .. }
            if valid_desktop_query_id(query_id) => {}
        // The same usage command and DTO the terminal page reads, so the two
        // surfaces cannot compute different numbers. Read-only, session-scoped,
        // and the calendar-day offset is bounded exactly as the runtime bounds
        // it.
        ClientCommand::QueryUsage {
            query_id,
            utc_offset_minutes,
            ..
        } if valid_desktop_query_id(query_id) && (-1440..=1440).contains(utc_offset_minutes) => {}
        ClientCommand::GetAgent { query_id, name, .. }
            if valid_desktop_query_id(query_id)
                && leveler_agent::agent_registry::validate_agent_name(name).is_ok() => {}
        // Skill reads are the runtime's registry, not a Desktop discovery
        // path. The bridge bounds the request shape; names, trust state and
        // the loaded content are the runtime's answer.
        ClientCommand::ListSkills { query_id, .. } if valid_desktop_query_id(query_id) => {}
        ClientCommand::GetSkill { query_id, name, .. }
            if valid_desktop_query_id(query_id) && valid_desktop_skill_name(name) => {}
        ClientCommand::QueryObservability {
            query_id,
            center_seq,
            before,
            after,
            ..
        } if valid_desktop_query_id(query_id)
            && center_seq.is_none_or(|seq| (0..=9_007_199_254_740_991).contains(&seq))
            && *before <= 100
            && *after <= 100 => {}
        ClientCommand::RequestDiff { query_id, .. }
            if query_id
                .as_ref()
                .is_none_or(|id| !id.as_str().trim().is_empty() && id.as_str().len() <= 256) => {}
        // Scheduled automation is a structured adapter over the schedule store:
        // the runtime answers `ScheduleManaged` on the requester session's
        // stream, and the bridge only bounds the payload. Definition semantics
        // (empty name, unknown rule, stale revision) stay owned by the
        // schedule manager so every client sees the same refusal.
        ClientCommand::ManageSchedule {
            query_id, request, ..
        } if valid_desktop_command_id(query_id) && valid_desktop_schedule_request(request) => {}
        // MCP management is the runtime's one authority. The bridge bounds the
        // declaration and binds it to the selected session; names, references,
        // URLs and transport semantics are validated again by the runtime.
        ClientCommand::ManageMcp {
            requester_session_id,
            query_id,
            request,
        } if requester_session_id == session
            && valid_desktop_command_id(query_id)
            && valid_desktop_mcp_request(request) => {}
        ClientCommand::QueryToolOutput {
            query_id,
            call_id,
            index,
            offset,
            max_bytes,
            ..
        } if valid_desktop_command_id(query_id)
            && valid_desktop_tool_call_id(call_id)
            && index.is_none_or(|index| index <= 1024)
            && *offset <= 1024 * 1024 * 1024
            && (1..=leveler_client_protocol::TOOL_OUTPUT_PAGE_MAX_BYTES).contains(max_bytes) => {}
        ClientCommand::ResumeTask { content, .. }
            if !content.is_empty() && content.chars().count() <= 100_000 => {}
        ClientCommand::CancelBackgroundTask { task_id, .. }
            if !task_id.trim().is_empty() && task_id.len() <= 256 => {}
        // A trust answer must name the session that owns the waiter and a
        // bounded request identity. The runtime still refuses a request it no
        // longer holds; the bridge refuses an unroutable answer earlier.
        ClientCommand::DefinitionTrustDecision {
            request_id,
            session_id: Some(_),
            ..
        } if !request_id.as_str().trim().is_empty() && request_id.as_str().len() <= 256 => {}
        // Credentials travel through the one admission transform every client
        // uses. The bridge checks shape and bounds; the runtime owns custody,
        // placeholder replacement and the plaintext acknowledgement.
        ClientCommand::SubmitUserInput {
            content,
            attachments,
            secret_captures,
            plaintext_secret_ack,
            ..
        } if content.chars().count() <= 100_000
            && attachments.len() <= 16
            && attachments.iter().all(valid_desktop_image)
            && valid_desktop_secret_captures(content, secret_captures, *plaintext_secret_ack)
            && (!content.is_empty() || !attachments.is_empty()) => {}
        ClientCommand::SteerUserInput {
            content,
            secret_captures,
            plaintext_secret_ack,
            message_id,
            ..
        } if !content.is_empty()
            && content.chars().count() <= 100_000
            && valid_desktop_secret_captures(content, secret_captures, *plaintext_secret_ack)
            && message_id
                .as_ref()
                .is_none_or(|id| !id.as_str().trim().is_empty() && id.as_str().len() <= 256) => {}
        ClientCommand::CaptureSecret {
            request_id,
            value,
            key_hint,
            ..
        } if !request_id.as_str().trim().is_empty()
            && request_id.as_str().len() <= 256
            && !value.is_empty()
            && value.len() <= MAX_DESKTOP_SECRET_BYTES
            && key_hint.as_ref().is_none_or(|hint| {
                !hint.is_empty() && hint.len() <= MAX_DESKTOP_SECRET_HINT_BYTES
            }) => {}
        ClientCommand::RevokeSecret { secret_ref, .. } if valid_desktop_secret_ref(secret_ref) => {}
        // The product collaboration axis is the same typed command the
        // terminal sends. `work_profile` is the deprecated wire field every
        // current client fills with `single`; it carries no product meaning and
        // the runtime ignores it. The axis itself is the runtime's to validate
        // again — the bridge only refuses values that are not representable.
        ClientCommand::SetProductAxes {
            work_profile,
            collaboration,
            ..
        } if !work_profile.is_empty()
            && work_profile.len() <= 64
            && matches!(collaboration.as_str(), "chat" | "plan" | "goal") => {}
        ClientCommand::SteerCurrentTurn { .. }
        | ClientCommand::CancelCurrentTurn { .. }
        | ClientCommand::CancelTask { .. }
        | ClientCommand::QuerySessionHistory { .. }
        | ClientCommand::SelectModel { .. }
        | ClientCommand::SetPermissionProfile { .. }
        | ClientCommand::RenameSession { .. }
        | ClientCommand::ArchiveSession { .. }
        | ClientCommand::UnarchiveSession { .. }
        | ClientCommand::ForkSession { .. }
        | ClientCommand::CompactContext { .. }
        | ClientCommand::ApprovalDecision { .. }
        | ClientCommand::AnswerClarification { .. } => {}
        _ => anyhow::bail!("command is outside supported desktop scope"),
    }
    if command.session_id().is_some_and(|id| id != session) {
        anyhow::bail!("command session does not match envelope");
    }
    Ok(())
}

fn valid_desktop_query_id(id: &Option<leveler_core::CommandId>) -> bool {
    id.as_ref().is_some_and(valid_desktop_command_id)
}

/// The loose shape a discovered skill directory must have to be addressable.
/// This mirrors `leveler_skills::validate_read_name`'s boundary (a single,
/// non-hidden, referenceable component) without importing the registry into
/// the bridge; the runtime validates the exact name again.
fn valid_desktop_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// A tool-call identity is a provider/host token, not free prose: the query
/// only ever looks it up, but bounding it keeps an answer from being built
/// around a meaningless key.
fn valid_desktop_tool_call_id(call_id: &str) -> bool {
    !call_id.trim().is_empty()
        && call_id.len() <= 256
        && call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn valid_desktop_command_id(id: &leveler_core::CommandId) -> bool {
    !id.as_str().trim().is_empty() && id.as_str().len() <= 256
}

const MAX_DESKTOP_MCP_COMMAND: usize = 4096;
const MAX_DESKTOP_MCP_ARG: usize = 8192;
const MAX_DESKTOP_MCP_ARGS: usize = 256;
const MAX_DESKTOP_MCP_REFS: usize = 64;
const MAX_DESKTOP_MCP_REF: usize = 256;
const MAX_DESKTOP_MCP_URL: usize = 2048;

fn valid_desktop_mcp_name(name: &str) -> bool {
    leveler_app::mcp_config::validate_name(name).is_ok()
}

fn valid_desktop_mcp_ref(reference: &leveler_client_protocol::mcp::McpNameRef) -> bool {
    !reference.key.is_empty()
        && reference.key.len() <= MAX_DESKTOP_MCP_REF
        && !reference.source.is_empty()
        && reference.source.len() <= MAX_DESKTOP_MCP_REF
}

fn valid_desktop_mcp_spec(spec: &McpServerSpec) -> bool {
    if !valid_desktop_mcp_name(&spec.name) {
        return false;
    }
    match spec.transport {
        McpTransport::Stdio => {
            spec.command.as_deref().is_some_and(|command| {
                !command.trim().is_empty() && command.len() <= MAX_DESKTOP_MCP_COMMAND
            }) && spec.args.len() <= MAX_DESKTOP_MCP_ARGS
                && spec
                    .args
                    .iter()
                    .all(|argument| argument.len() <= MAX_DESKTOP_MCP_ARG)
                && spec.env.len() <= MAX_DESKTOP_MCP_REFS
                && spec.env.iter().all(valid_desktop_mcp_ref)
                && spec.url.is_none()
                && spec.headers.is_empty()
                && spec.bearer_token_env.is_none()
        }
        McpTransport::StreamableHttp => {
            let url = spec.url.as_deref().unwrap_or("");
            !url.trim().is_empty()
                && url.len() <= MAX_DESKTOP_MCP_URL
                && leveler_app::mcp_config::validate_streamable_http_url(url).is_ok()
                && spec.command.is_none()
                && spec.args.is_empty()
                && spec.env.is_empty()
                && spec.headers.len() <= MAX_DESKTOP_MCP_REFS
                && spec.headers.iter().all(valid_desktop_mcp_ref)
                && spec
                    .bearer_token_env
                    .as_ref()
                    .is_none_or(|source| !source.is_empty() && source.len() <= MAX_DESKTOP_MCP_REF)
        }
    }
}

fn valid_desktop_mcp_request(request: &McpRequest) -> bool {
    match request {
        McpRequest::List | McpRequest::Reload => true,
        McpRequest::Reconnect { server } | McpRequest::Remove { server } => {
            valid_desktop_mcp_name(server)
        }
        McpRequest::SetEnabled { server, .. } => valid_desktop_mcp_name(server),
        McpRequest::Add { spec } => valid_desktop_mcp_spec(spec),
        McpRequest::Update {
            original_name,
            spec,
        } => valid_desktop_mcp_name(original_name) && valid_desktop_mcp_spec(spec),
    }
}

const MAX_DESKTOP_SCHEDULE_ID: usize = 256;
const MAX_DESKTOP_SCHEDULE_NAME: usize = 1_024;
const MAX_DESKTOP_SCHEDULE_PATH: usize = 4_096;
const MAX_DESKTOP_SCHEDULE_FIELD: usize = 256;
const MAX_DESKTOP_SCHEDULE_TEXT: usize = 100_000;
const MAX_DESKTOP_SCHEDULE_RUNS: u32 = 500;

fn valid_desktop_schedule_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= MAX_DESKTOP_SCHEDULE_ID
}

fn valid_desktop_schedule_field(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.len() <= MAX_DESKTOP_SCHEDULE_FIELD)
}

fn valid_desktop_schedule_action(action: &ScheduleActionWire) -> bool {
    match action {
        ScheduleActionWire::Agent { goal } => {
            !goal.trim().is_empty() && goal.len() <= MAX_DESKTOP_SCHEDULE_TEXT
        }
        ScheduleActionWire::Shell { command } => {
            !command.trim().is_empty() && command.len() <= MAX_DESKTOP_SCHEDULE_TEXT
        }
        ScheduleActionWire::Native { action, input } => {
            !action.trim().is_empty()
                && action.len() <= MAX_DESKTOP_SCHEDULE_FIELD
                && input.len() <= MAX_DESKTOP_SCHEDULE_TEXT
        }
    }
}

fn valid_desktop_schedule_trigger(trigger: &ScheduleTriggerWire) -> bool {
    match trigger {
        ScheduleTriggerWire::Once { at } => {
            !at.trim().is_empty() && at.len() <= MAX_DESKTOP_SCHEDULE_FIELD
        }
        ScheduleTriggerWire::Every { every_secs, anchor } => {
            *every_secs > 0 && valid_desktop_schedule_field(anchor.as_deref())
        }
        ScheduleTriggerWire::Daily { time, timezone } => {
            !time.trim().is_empty()
                && time.len() <= MAX_DESKTOP_SCHEDULE_FIELD
                && valid_desktop_schedule_field(timezone.as_deref())
        }
        ScheduleTriggerWire::Weekly {
            weekdays,
            time,
            timezone,
        } => {
            !weekdays.trim().is_empty()
                && weekdays.len() <= MAX_DESKTOP_SCHEDULE_FIELD
                && !time.trim().is_empty()
                && time.len() <= MAX_DESKTOP_SCHEDULE_FIELD
                && valid_desktop_schedule_field(timezone.as_deref())
        }
        ScheduleTriggerWire::Cron {
            expression,
            timezone,
        } => {
            !expression.trim().is_empty()
                && expression.len() <= MAX_DESKTOP_SCHEDULE_FIELD
                && valid_desktop_schedule_field(timezone.as_deref())
        }
    }
}

fn valid_desktop_schedule_draft(draft: &ScheduleDraft) -> bool {
    !draft.name.trim().is_empty()
        && draft.name.len() <= MAX_DESKTOP_SCHEDULE_NAME
        && !draft.workspace.trim().is_empty()
        && draft.workspace.len() <= MAX_DESKTOP_SCHEDULE_PATH
        && valid_desktop_schedule_action(&draft.action)
        && valid_desktop_schedule_trigger(&draft.trigger)
        && valid_desktop_schedule_field(draft.session_policy.as_deref())
        && valid_desktop_schedule_field(draft.overlap.as_deref())
        && valid_desktop_schedule_field(draft.mode.as_deref())
        && valid_desktop_schedule_field(draft.model.as_deref())
        && valid_desktop_schedule_field(draft.misfire.as_deref())
}

fn valid_desktop_schedule_patch(patch: &SchedulePatch) -> bool {
    patch
        .name
        .as_ref()
        .is_none_or(|name| name.len() <= MAX_DESKTOP_SCHEDULE_NAME)
        && patch
            .workspace
            .as_ref()
            .is_none_or(|workspace| workspace.len() <= MAX_DESKTOP_SCHEDULE_PATH)
        && patch
            .action
            .as_ref()
            .is_none_or(valid_desktop_schedule_action)
        && patch
            .trigger
            .as_ref()
            .is_none_or(valid_desktop_schedule_trigger)
        && valid_desktop_schedule_field(patch.session_policy.as_deref())
        && valid_desktop_schedule_field(patch.overlap.as_deref())
        && valid_desktop_schedule_field(patch.mode.as_deref())
        && valid_desktop_schedule_field(patch.model.as_deref())
        && valid_desktop_schedule_field(patch.misfire.as_deref())
}

fn valid_desktop_schedule_request(request: &ScheduleRequest) -> bool {
    match request {
        ScheduleRequest::List { .. }
        | ScheduleRequest::Status
        | ScheduleRequest::WakeupEnable
        | ScheduleRequest::WakeupDisable
        | ScheduleRequest::WakeupDoctor => true,
        ScheduleRequest::Show { id }
        | ScheduleRequest::Pause { id }
        | ScheduleRequest::Resume { id }
        | ScheduleRequest::RunNow { id }
        | ScheduleRequest::Cancel { id }
        | ScheduleRequest::Remove { id }
        | ScheduleRequest::RevokeGrant { id }
        | ScheduleRequest::Grant { id, .. } => valid_desktop_schedule_id(id),
        ScheduleRequest::Create { draft } => valid_desktop_schedule_draft(draft),
        ScheduleRequest::Update {
            id,
            expected_revision,
            patch,
        } => {
            valid_desktop_schedule_id(id)
                && expected_revision.as_ref().is_none_or(|revision| {
                    !revision.trim().is_empty() && revision.len() <= MAX_DESKTOP_SCHEDULE_FIELD
                })
                && valid_desktop_schedule_patch(patch)
        }
        ScheduleRequest::Runs { definition, limit } => {
            definition
                .as_ref()
                .is_none_or(|id| valid_desktop_schedule_id(id))
                && limit.is_none_or(|limit| (1..=MAX_DESKTOP_SCHEDULE_RUNS).contains(&limit))
        }
    }
}

fn valid_desktop_image(attachment: &leveler_client_protocol::AttachmentRef) -> bool {
    attachment.kind == leveler_client_protocol::AttachmentKind::Image
        && attachment.mime_type == "image/png"
        && !attachment.id.as_str().is_empty()
        && attachment.id.as_str().len() <= 256
        && !attachment.name.is_empty()
        && attachment.name.len() <= 1024
        && attachment.size_bytes <= 20 * 1024 * 1024
        && attachment.sha256.len() == 64
        && attachment
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && attachment
            .width
            .is_some_and(|width| (1..=2048).contains(&width))
        && attachment
            .height
            .is_some_and(|height| (1..=2048).contains(&height))
}

/// One message may hand at most this many credentials. A message with more is
/// a data dump, not a request (mirrors the runtime's admission bound).
const MAX_DESKTOP_CAPTURES: usize = 16;
const MAX_DESKTOP_PLACEHOLDER_BYTES: usize = 256;
const MAX_DESKTOP_SECRET_BYTES: usize = 64 * 1024;
const MAX_DESKTOP_SECRET_HINT_BYTES: usize = 128;

/// A capture is well-formed only when every placeholder occupies exactly one
/// span of the message and no two captures share one. The runtime revalidates;
/// this boundary refuses malformed shapes before they can reach the vault.
fn valid_desktop_secret_captures(
    content: &str,
    captures: &[leveler_client_protocol::secret::SecretCapture],
    plaintext_ack: bool,
) -> bool {
    // Capture and plaintext are different decisions for the same bytes; the
    // runtime refuses the combination, so the adapter does too.
    if plaintext_ack && !captures.is_empty() {
        return false;
    }
    if captures.len() > MAX_DESKTOP_CAPTURES {
        return false;
    }
    let mut seen = std::collections::BTreeSet::new();
    for capture in captures {
        if capture.placeholder.is_empty()
            || capture.placeholder.len() > MAX_DESKTOP_PLACEHOLDER_BYTES
            || capture.value.is_empty()
            || capture.value.len() > MAX_DESKTOP_SECRET_BYTES
            || capture
                .key_hint
                .as_ref()
                .is_some_and(|hint| hint.is_empty() || hint.len() > MAX_DESKTOP_SECRET_HINT_BYTES)
            || !seen.insert(capture.placeholder.as_str())
            || content.matches(capture.placeholder.as_str()).count() != 1
        {
            return false;
        }
    }
    true
}

/// `sec_` plus 128 random bits as lowercase hex — the vault's one reference
/// spelling. Anything else is not a reference this host may ask to revoke.
fn valid_desktop_secret_ref(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sec_") else {
        return false;
    };
    hex.len() == 32
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The configured model refs, read through the same config authority the
/// runtime uses. No session and no runtime process are created.
fn model_list_payload(layout: Layout) -> anyhow::Result<Value> {
    let config = leveler_app::ConfigManager::load(layout)?;
    let mut models = config.snapshot().registry.model_refs();
    models.sort_by_key(|model| model.to_string());
    Ok(json!({ "models": models }))
}

/// The host-side credential scan the Desktop UI uses for its review. It reuses
/// the exact detector the TUI and the runtime admission use, and returns only
/// masks, names and spans — never the value.
fn secret_scan_payload(content: &str) -> Value {
    use leveler_core::SecretLevel;
    let findings = leveler_core::detect_secrets(content);
    // The value is not an instruction: blank its span before deriving the use
    // scope, exactly as the runtime does.
    let mut scope_text = content.to_string();
    for finding in findings.iter().rev() {
        if finding.start < finding.end && finding.end <= scope_text.len() {
            scope_text.replace_range(
                finding.start..finding.end,
                &" ".repeat(finding.end - finding.start),
            );
        }
    }
    let items = findings
        .iter()
        .map(|finding| {
            // Byte offsets are Rust-side; the renderer slices UTF-16. Both are
            // reported so a client never has to convert one into the other.
            let start_utf16 = content[..finding.start].encode_utf16().count();
            let end_utf16 = content[..finding.end].encode_utf16().count();
            json!({
                "start": finding.start,
                "end": finding.end,
                "start_utf16": start_utf16,
                "end_utf16": end_utf16,
                "level": match finding.level {
                    SecretLevel::HighConfidence => "high_confidence",
                    SecretLevel::Ambiguous => "ambiguous",
                },
                "kind": format!("{:?}", finding.kind),
                "key": finding.key,
                "masked": leveler_core::masked_secret(finding.value(content)),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "findings": items,
        "use_targets": leveler_core::dotenv_targets_in(&scope_text),
        "use_fields": leveler_core::dotenv_fields_in(&scope_text),
    })
}

struct Bridge {
    home: leveler_core::LevelerHome,
    config_dir: Option<PathBuf>,
    client: Option<Arc<LocalSocketRuntimeClient>>,
    layout: Option<Layout>,
    selected: Option<SessionId>,
    terminal_clients: std::collections::HashMap<(SessionId, String), Arc<LocalSocketRuntimeClient>>,
    subscription: Option<JoinHandle<()>>,
    output: mpsc::Sender<Value>,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.clear_subscription();
    }
}
impl Bridge {
    fn clear_subscription(&mut self) {
        if let Some(handle) = self.subscription.take() {
            handle.abort();
        }
        self.selected = None;
    }
    fn launch(&self) -> anyhow::Result<DetachedRuntimeLaunch> {
        Ok(DetachedRuntimeLaunch {
            executable: std::env::current_exe()?,
            ready_prefix: "leveler-desktop-ready".into(),
        })
    }
    fn layout_for(&self, workspace: Option<&str>) -> anyhow::Result<Layout> {
        match workspace {
            None => Ok(Layout::no_workspace(
                self.home.clone(),
                self.config_dir.clone(),
            )),
            Some(path) => {
                let root = std::fs::canonicalize(path)?;
                anyhow::ensure!(root.is_dir(), "workspace must be a directory");
                Ok(Layout::ephemeral(
                    root,
                    self.config_dir.clone(),
                    self.home.root(),
                ))
            }
        }
    }
    async fn install(
        &mut self,
        client: LocalSocketRuntimeClient,
        layout: Layout,
    ) -> anyhow::Result<Value> {
        let info = LocalRuntimeService::runtime_info(&client).await?;
        client.set_reviver(Arc::new(DaemonReviver::new(
            layout.clone(),
            self.launch()?,
            Arc::new(DesktopHandoff(self.output.clone())),
        )));
        self.clear_subscription();
        self.client = Some(Arc::new(client));
        self.layout = Some(layout.clone());
        Ok(json!({"runtime_info": info, "source_id": layout.database_path().display().to_string()}))
    }
    fn client(&self) -> anyhow::Result<Arc<LocalSocketRuntimeClient>> {
        self.client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("connect a runtime first"))
    }
    fn watch(&mut self, session: SessionId) -> anyhow::Result<()> {
        self.clear_subscription();
        let client = self.client()?;
        let mut events = client.subscribe_session(&session);
        let output = self.output.clone();
        self.selected = Some(session.clone());
        self.subscription = Some(tokio::spawn(async move {
            loop {
                let payload = match events.recv().await {
                    Ok(mut event) => {
                        // Desktop submits opaque refs; local preview paths are
                        // terminal-only metadata, never renderer authority.
                        if let leveler_client_protocol::RuntimeEvent::AttachmentAdded {
                            attachment,
                            ..
                        } = &mut event
                        {
                            attachment.source_path = None;
                            attachment.preview_path = None;
                        }
                        json!({"event":"runtime", "session_id":session, "data":event})
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        match client.snapshot(&session).await {
                            Ok(snapshot) => {
                                json!({"event":"snapshot", "session_id":session, "data":snapshot})
                            }
                            Err(error) => {
                                json!({"event":"error", "session_id":session, "error":{"message":error.to_string(),"kind":"resync"}})
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if output.send(payload).await.is_err() {
                    break;
                }
            }
        }));
        Ok(())
    }
    async fn terminal_client(
        &mut self,
        session: &SessionId,
        terminal_id: &str,
    ) -> anyhow::Result<Arc<LocalSocketRuntimeClient>> {
        let key = (session.clone(), terminal_id.to_owned());
        if let Some(client) = self.terminal_clients.get(&key) {
            return Ok(client.clone());
        }
        // The index locates an existing owner; it cannot grant execution. The
        // runtime revalidates the durable session and the host checks PTY ownership.
        // No install/watch call here: terminal I/O must not change UI selection.
        let index = leveler_app::global_task_index::query_global_tasks(&self.home, true).await;
        anyhow::ensure!(
            index.source_errors.is_empty(),
            "terminal source discovery is incomplete"
        );
        let mut sources = index.tasks.iter().filter(|task| &task.id == session);
        let source = sources
            .next()
            .ok_or_else(|| anyhow::anyhow!("terminal session is not registered"))?;
        anyhow::ensure!(
            sources.next().is_none(),
            "terminal session has ambiguous source ownership"
        );
        let client = leveler_runtime_host::connect_global_task_runtime(
            &self.home,
            &source.source_id,
            session,
            self.config_dir.clone(),
            &self.launch()?,
            Arc::new(DesktopHandoff(self.output.clone())),
        )
        .await?;
        let layout = self.layout_for(source.primary_workspace.as_deref())?;
        client.set_reviver(Arc::new(DaemonReviver::new(
            layout,
            self.launch()?,
            Arc::new(DesktopHandoff(self.output.clone())),
        )));
        let client = Arc::new(client);
        self.terminal_clients.insert(key, client.clone());
        Ok(client)
    }
    /// Start one delivery without blocking the bridge's read loop.
    ///
    /// A command can be interactive: an explicit `/skill` send parks inside
    /// `SubmitMessage` until the same client answers its definition-trust
    /// request. The bridge is the only path that answer can travel, so awaiting
    /// the send on the read loop would deadlock the waiter it is waiting for.
    /// The runtime remains the authority for ordering and ownership; this only
    /// lets the bridge relay the next line while an earlier command is open.
    fn spawn_delivery(
        &self,
        params: Value,
        id: Value,
        output: mpsc::Sender<Value>,
    ) -> anyhow::Result<()> {
        let envelope: CommandEnvelope = serde_json::from_value(params["envelope"].clone())?;
        anyhow::ensure!(
            self.selected.as_ref() == Some(&envelope.session_id),
            "session is not selected"
        );
        validate_command(&envelope.session_id, &envelope.command)?;
        let client = self.client()?;
        tokio::spawn(async move {
            let response = match client.deliver(envelope).await {
                Ok(()) => json!({"id": id, "ok": true, "result": Value::Null}),
                Err(error) => json!({
                    "id": id,
                    "ok": false,
                    "error": error_payload(&anyhow::Error::new(error)),
                }),
            };
            let _ = output.send(response).await;
        });
        Ok(())
    }

    async fn handle(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        match method {
            // Transport liveness only. Desktop uses it to prove a respawned
            // bridge answers before announcing a connection; it touches no
            // runtime, session or durable state.
            "ping" => Ok(json!({"ok": true})),
            // Read-only credential review for the Desktop composer. It reuses
            // the runtime's detector and never sends the value back.
            "secret_scan" => {
                let content = required_string(&params, "content")?;
                anyhow::ensure!(
                    content.chars().count() <= 100_000,
                    "content exceeds the message limit"
                );
                Ok(secret_scan_payload(content))
            }
            // A session-less read of the configured models for the composer's
            // picker. The list is the same registry the snapshot advertises;
            // selecting one only becomes a fact when a session is created.
            "models" => {
                let workspace = workspace_param(&params)?;
                model_list_payload(self.layout_for(workspace)?)
            }
            // Provider management is host-side user configuration, not
            // runtime state: the same file `leveler login` edits, read and
            // written through the one shared service. The renderer only ever
            // receives masked facts; a key it sends is written and forgotten.
            "providers" => {
                let layout = self.layout_for(workspace_param(&params)?)?;
                crate::desktop_provider::view(layout, self.client.as_ref(), self.layout.as_ref())
                    .await
            }
            "provider_save" => {
                let layout = self.layout_for(workspace_param(&params)?)?;
                crate::desktop_provider::save(
                    layout,
                    self.client.as_ref(),
                    self.layout.as_ref(),
                    &params,
                )
                .await
            }
            "provider_remove" => {
                let layout = self.layout_for(workspace_param(&params)?)?;
                crate::desktop_provider::remove(
                    layout,
                    self.client.as_ref(),
                    self.layout.as_ref(),
                    &params,
                )
                .await
            }
            // A saved provider or a not-yet-saved draft: first-run must be
            // able to verify a key before it is written anywhere.
            "provider_models" => {
                let layout = self.layout_for(workspace_param(&params)?)?;
                crate::desktop_provider::models(&layout, &params).await
            }
            // A real text + streaming request through the configured route.
            "provider_test" => {
                let layout = self.layout_for(workspace_param(&params)?)?;
                crate::desktop_provider::test(&layout, &params).await
            }
            "default_model_set" => {
                let layout = self.layout_for(workspace_param(&params)?)?;
                crate::desktop_provider::set_default_model(
                    layout,
                    self.client.as_ref(),
                    self.layout.as_ref(),
                    &params,
                )
                .await
            }
            // Explicit apply of the current files to the attached runtime.
            "runtime_reload" => Ok(crate::desktop_provider::reload(self.client.as_ref()).await),
            "list_tasks" => Ok(serde_json::to_value(
                leveler_app::global_task_index::query_global_tasks(
                    &self.home,
                    params["include_archived"].as_bool().unwrap_or(false),
                )
                .await,
            )?),
            "connect" => {
                let layout = self.layout_for(workspace_param(&params)?)?;
                let client = leveler_runtime_host::ensure_default_runtime(
                    &layout,
                    &self.launch()?,
                    Arc::new(DesktopHandoff(self.output.clone())),
                )
                .await?;
                self.install(client, layout).await
            }
            "open" => {
                let source = required_string(&params, "source_id")?;
                let session = SessionId::new(required_string(&params, "session_id")?);
                let (_, workspace) = leveler_app::global_task_index::resolve_global_task_source(
                    &self.home, source, &session,
                )
                .await?;
                let layout = self.layout_for(workspace.as_deref())?;
                let client = leveler_runtime_host::connect_global_task_runtime(
                    &self.home,
                    source,
                    &session,
                    self.config_dir.clone(),
                    &self.launch()?,
                    Arc::new(DesktopHandoff(self.output.clone())),
                )
                .await?;
                let mut result = self.install(client, layout).await?;
                self.watch(session.clone())?;
                result["session"] = serde_json::to_value(self.client()?.snapshot(&session).await?)?;
                Ok(result)
            }
            "create" | "create_identified" => {
                let request_id = params
                    .get("request_id")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .map(serde_json::from_value::<leveler_client_protocol::CommandId>)
                    .transpose()?;
                if method == "create_identified"
                    && request_id.as_ref().is_none_or(|id| id.as_str().is_empty())
                {
                    anyhow::bail!("identified creation requires a nonempty request_id");
                }
                let client = self.client()?;
                let workspace = self
                    .layout
                    .as_ref()
                    .and_then(|layout| layout.primary_workspace());
                let request = CreateSessionRequest {
                    request_id,
                    permission_contract: None,
                    collaboration: requested_collaboration(&params)?,
                    workspace: workspace.map_or(CreateWorkspaceSelection::None, |root| {
                        CreateWorkspaceSelection::Workspace {
                            path: root.display().to_string(),
                        }
                    }),
                    goal: params["goal"].as_str().unwrap_or("New task").to_owned(),
                    model: params
                        .get("model")
                        .filter(|value| !value.is_null())
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()?,
                    mode: params
                        .get("mode")
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()?
                        .unwrap_or(leveler_client_protocol::PermissionProfile::Assisted),
                    approval_policy: leveler_client_protocol::ApprovalPolicy::Interactive,
                };
                let bootstrap = client.create_session(request).await?;
                self.watch(bootstrap.session.id.clone())?;
                Ok(serde_json::to_value(bootstrap)?)
            }
            "snapshot" => {
                let session = SessionId::new(required_string(&params, "session_id")?);
                anyhow::ensure!(
                    self.selected.as_ref() == Some(&session),
                    "session is not selected"
                );
                Ok(serde_json::to_value(
                    self.client()?.snapshot(&session).await?,
                )?)
            }
            "create_terminal" | "write_terminal" | "resize_terminal" | "observe_terminal"
            | "close_terminal" => {
                use base64::Engine as _;
                use leveler_client_protocol::{TerminalOperation, TerminalRequest};
                let session_id = SessionId::new(required_string(&params, "session_id")?);
                let dimension = |key: &str| -> anyhow::Result<u16> {
                    params[key]
                        .as_u64()
                        .and_then(|n| u16::try_from(n).ok())
                        .ok_or_else(|| anyhow::anyhow!("{key} must be an integer"))
                };
                let operation = match method {
                    "create_terminal" => TerminalOperation::Create {
                        cols: dimension("cols")?,
                        rows: dimension("rows")?,
                    },
                    "resize_terminal" => TerminalOperation::Resize {
                        cols: dimension("cols")?,
                        rows: dimension("rows")?,
                    },
                    "write_terminal" => {
                        let encoded = required_string(&params, "data_base64")?;
                        anyhow::ensure!(encoded.len() <= 87384, "terminal input exceeds limit");
                        TerminalOperation::Write {
                            data: base64::engine::general_purpose::STANDARD.decode(encoded)?,
                        }
                    }
                    "observe_terminal" => TerminalOperation::Observe {
                        cursor: params["cursor"]
                            .as_u64()
                            .ok_or_else(|| anyhow::anyhow!("cursor must be an unsigned integer"))?,
                        max_bytes: params["max_bytes"]
                            .as_u64()
                            .and_then(|n| usize::try_from(n).ok())
                            .ok_or_else(|| {
                                anyhow::anyhow!("max_bytes must be an unsigned integer")
                            })?,
                    },
                    _ => TerminalOperation::Close,
                };
                let request = TerminalRequest {
                    session_id: session_id.clone(),
                    terminal_id: required_string(&params, "terminal_id")?.into(),
                    operation,
                };
                request.validate().map_err(anyhow::Error::msg)?;
                let client = self
                    .terminal_client(&request.session_id, &request.terminal_id)
                    .await?;
                match client.terminal(request).await? {
                    None => Ok(Value::Null),
                    Some(mut state) => {
                        if method == "close_terminal"
                            && matches!(state.status.as_str(), "exited" | "killed")
                        {
                            self.terminal_clients
                                .remove(&(session_id, state.terminal_id.clone()));
                        }
                        let bytes = std::mem::take(&mut state.data);
                        let mut response = serde_json::to_value(state)?;
                        response.as_object_mut().unwrap().remove("data");
                        response["data_base64"] =
                            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
                        Ok(response)
                    }
                }
            }
            "runtime_info" => Ok(serde_json::to_value(
                LocalRuntimeService::runtime_info(self.client()?.as_ref()).await?,
            )?),
            _ => anyhow::bail!("unknown desktop method: {method}"),
        }
    }
}
fn required_string<'a>(params: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    params[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{key} must be a nonempty string"))
}

pub(crate) async fn run(config_dir: Option<PathBuf>) -> anyhow::Result<std::process::ExitCode> {
    let (output, mut receiver) = mpsc::channel::<Value>(256);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = receiver.recv().await {
            let mut line = serde_json::to_vec(&value)?;
            line.push(b'\n');
            stdout.write_all(&line).await?;
            stdout.flush().await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let mut bridge = Bridge {
        home: leveler_core::LevelerHome::resolve(leveler_core::environment()),
        config_dir,
        client: None,
        layout: None,
        selected: None,
        terminal_clients: Default::default(),
        subscription: None,
        output: output.clone(),
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) if request.method == "deliver" => {
                let id = request.id.clone();
                match bridge.spawn_delivery(request.params, id.clone(), output.clone()) {
                    // The task writes this request's response when the runtime
                    // settles it; keep reading so an interactive command can
                    // receive the decision it is waiting for.
                    Ok(()) => continue,
                    Err(error) => json!({"id":id,"ok":false,"error":error_payload(&error)}),
                }
            }
            Ok(request) => match bridge.handle(&request.method, request.params).await {
                Ok(result) => json!({"id":request.id,"ok":true,"result":result}),
                Err(error) => json!({"id":request.id,"ok":false,"error":error_payload(&error)}),
            },
            Err(error) => {
                json!({"id":null,"ok":false,"error":{"message":error.to_string(),"kind":"invalid_request"}})
            }
        };
        if output.send(response).await.is_err() {
            break;
        }
    }
    // EOF only releases this desktop attachment. Never send Quit to the daemon.
    drop(bridge);
    drop(output);
    writer.await??;
    Ok(std::process::ExitCode::SUCCESS)
}
/// The typed lifecycle state of a failed request, when the failure is a runtime
/// lifecycle fact rather than a protocol error.
fn lifecycle_state(error: &anyhow::Error) -> Option<leveler_runtime_host::RuntimeLifecycleState> {
    error
        .downcast_ref::<leveler_runtime_host::EnsureError>()
        .map(leveler_runtime_host::RuntimeLifecycleState::from_ensure_error)
}

/// The structured error a Desktop client receives.
///
/// `kind` is the protocol error class; `state` and `next_step` are present only
/// for a lifecycle failure, and they come from the SAME vocabulary the terminal
/// uses — so a Desktop client can offer the actionable next step
/// instead of a generic "connect failed".
fn error_payload(error: &anyhow::Error) -> Value {
    let mut payload = json!({"message": error.to_string(), "kind": error_kind(error)});
    if let Some(state) = lifecycle_state(error) {
        payload["state"] = json!(state.as_str());
        payload["next_step"] = json!(state.next_step().as_str());
    }
    payload
}

fn error_kind(error: &anyhow::Error) -> &'static str {
    use leveler_client_protocol::ClientError;
    match error.downcast_ref::<ClientError>() {
        Some(ClientError::OutcomeUnknown(_)) => "outcome_unknown",
        Some(ClientError::RecoveryRequired(_)) => "recovery_required",
        Some(ClientError::Unresolvable(_)) => "unresolvable",
        Some(ClientError::OwnershipConflict(_)) => "ownership_conflict",
        Some(ClientError::SteerConflict(_)) => "steer_conflict",
        Some(ClientError::SessionNotFound(_)) => "session_not_found",
        Some(ClientError::Runtime(_)) => "rejected",
        None if error
            .downcast_ref::<leveler_local_transport::TransportError>()
            .is_some() =>
        {
            "transport"
        }
        None => "request",
    }
}
fn workspace_param(params: &Value) -> anyhow::Result<Option<&str>> {
    match params.get("workspace") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(path)) if !path.is_empty() => Ok(Some(path)),
        _ => anyhow::bail!("workspace must be null or a nonempty string"),
    }
}
/// The axis a Desktop create request asks for.
///
/// The Desktop has no axis selector yet, and it is an INTERACTIVE host: a task
/// that states no axis is a conversation — the ONE statement every interactive
/// entry point consumes
/// ([`leveler_local_transport::CollaborationMode::interactive_session`]).
/// `CollaborationMode::default()` would have created a Goal session while the
/// UI said `chat`, which is exactly the drift this names. A stated axis is
/// honored as-is (a client that asked for the goal lifecycle gets it).
fn requested_collaboration(
    params: &serde_json::Value,
) -> Result<leveler_local_transport::CollaborationMode, serde_json::Error> {
    match params.get("collaboration") {
        Some(value) if !value.is_null() => serde_json::from_value(value.clone()),
        _ => Ok(leveler_local_transport::CollaborationMode::interactive_session()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_queries_are_bounded_and_session_scoped() {
        let session = SessionId::new("desktop-session");
        let command = |call_id: &str, index: Option<u32>, offset: u64, max_bytes: u32| {
            ClientCommand::QueryToolOutput {
                session_id: session.clone(),
                query_id: leveler_core::CommandId::new("q1"),
                call_id: call_id.to_string(),
                index,
                offset,
                max_bytes,
            }
        };
        assert!(validate_command(&session, &command("call_abc-1", None, 0, 65_536)).is_ok());
        assert!(validate_command(&session, &command("call_abc-1", Some(3), 1024, 262_144)).is_ok());
        for bad in [
            command("", None, 0, 1024),
            command("a b", None, 0, 1024),
            command("../../etc/passwd", None, 0, 1024),
            command("call", None, 0, 0),
            command("call", None, 0, 262_145),
            command("call", Some(1025), 0, 1024),
            command("call", None, 1024 * 1024 * 1024 + 1, 1024),
        ] {
            assert!(validate_command(&session, &bad).is_err(), "{bad:?}");
        }
        let mut other_session = command("call", None, 0, 1024);
        if let ClientCommand::QueryToolOutput { session_id, .. } = &mut other_session {
            *session_id = SessionId::new("another");
        }
        assert!(validate_command(&session, &other_session).is_err());
    }

    #[test]
    fn desktop_existing_task_commands_validate_scope_and_resume_content() {
        let session = SessionId::new("desktop-session");
        for command in [
            ClientCommand::CancelTask {
                session_id: session.clone(),
            },
            ClientCommand::ResumeTask {
                session_id: session.clone(),
                content: "继续".into(),
            },
        ] {
            assert!(validate_command(&session, &command).is_ok());
            assert!(validate_command(&SessionId::new("other-session"), &command).is_err());
        }
        for content in [String::new(), "a".repeat(100_001)] {
            assert!(
                validate_command(
                    &session,
                    &ClientCommand::ResumeTask {
                        session_id: session.clone(),
                        content
                    }
                )
                .is_err()
            );
        }
        assert!(
            validate_command(
                &session,
                &ClientCommand::ResumeTask {
                    session_id: session.clone(),
                    content: "a".repeat(100_000)
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn definition_trust_decisions_are_scoped_to_the_selected_session() {
        let session = SessionId::new("desktop-session");
        let decision = || leveler_client_protocol::DefinitionTrustDecision::AllowOnce;
        for (id, expected) in [
            (session.clone(), true),
            (SessionId::new("other-session"), false),
        ] {
            let command = ClientCommand::DefinitionTrustDecision {
                request_id: leveler_core::DefinitionTrustRequestId::new("trust-1"),
                decision: decision(),
                session_id: Some(id),
            };
            assert_eq!(validate_command(&session, &command).is_ok(), expected);
        }
        // The runtime refuses a decision that does not name its session; the
        // bridge refuses it earlier instead of forwarding an unroutable answer.
        for session_id in [None, Some(SessionId::new(" "))] {
            let command = ClientCommand::DefinitionTrustDecision {
                request_id: leveler_core::DefinitionTrustRequestId::new("trust-1"),
                decision: decision(),
                session_id,
            };
            assert!(validate_command(&session, &command).is_err());
        }
        let empty = ClientCommand::DefinitionTrustDecision {
            request_id: leveler_core::DefinitionTrustRequestId::new(""),
            decision: decision(),
            session_id: Some(session.clone()),
        };
        assert!(validate_command(&session, &empty).is_err());
    }

    #[test]
    fn secret_scan_returns_utf16_spans_and_never_the_value() {
        let text = "请把 .env 的 API_KEY=sk-abcdefghijklmnop1234 写入";
        let payload = secret_scan_payload(text);
        let findings = payload["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "{payload}");
        let finding = &findings[0];
        let value = "sk-abcdefghijklmnop1234";
        let start = finding["start_utf16"].as_u64().unwrap() as usize;
        let end = finding["end_utf16"].as_u64().unwrap() as usize;
        let sliced: String = text
            .encode_utf16()
            .skip(start)
            .take(end - start)
            .map(|unit| char::from_u32(u32::from(unit)).unwrap())
            .collect();
        assert_eq!(sliced, value);
        assert_eq!(
            finding["masked"].as_str().unwrap(),
            leveler_core::masked_secret(value)
        );
        assert!(!payload.to_string().contains(value));
        assert!(
            !payload["use_targets"].as_array().unwrap().is_empty(),
            "the scan describes the declared dotenv target: {payload}"
        );
    }

    #[test]
    fn desktop_secret_commands_validate_shape_bounds_and_session() {
        use leveler_client_protocol::secret::{SecretCapture, SecretValue};
        let capture = SecretCapture::new("{{s}}", "sk-x");
        assert!(valid_desktop_secret_captures(
            "use {{s}}",
            std::slice::from_ref(&capture),
            false
        ));
        assert!(!valid_desktop_secret_captures(
            "no placeholder",
            std::slice::from_ref(&capture),
            false
        ));
        assert!(!valid_desktop_secret_captures(
            "{{s}} and {{s}}",
            std::slice::from_ref(&capture),
            false
        ));
        assert!(!valid_desktop_secret_captures(
            "use {{s}}",
            &[capture.clone(), capture.clone()],
            false
        ));
        assert!(!valid_desktop_secret_captures(
            "use {{s}}",
            std::slice::from_ref(&capture),
            true
        ));
        assert!(valid_desktop_secret_ref(
            "sec_00000000000000000000000000000000"
        ));
        assert!(!valid_desktop_secret_ref("sec_xyz"));

        let session = SessionId::new("desktop-session");
        let submit = |id: SessionId| ClientCommand::SubmitUserInput {
            session_id: id,
            content: "hello".into(),
            attachments: vec![],
            secret_captures: vec![],
            plaintext_secret_ack: false,
        };
        assert!(validate_command(&session, &submit(session.clone())).is_ok());
        assert!(validate_command(&session, &submit(SessionId::new("other"))).is_err());
        let capture = ClientCommand::CaptureSecret {
            session_id: session.clone(),
            request_id: leveler_core::CommandId::new("r1"),
            value: SecretValue::new("sk-x"),
            key_hint: None,
        };
        assert!(validate_command(&session, &capture).is_ok());
        let revoke = ClientCommand::RevokeSecret {
            session_id: session.clone(),
            secret_ref: "sec_00000000000000000000000000000000".into(),
        };
        assert!(validate_command(&session, &revoke).is_ok());
        let bad_ref = ClientCommand::RevokeSecret {
            session_id: session.clone(),
            secret_ref: "not-a-reference".into(),
        };
        assert!(validate_command(&session, &bad_ref).is_err());
    }

    #[test]
    fn model_list_reads_configured_models_without_a_session() {
        let dir =
            std::env::temp_dir().join(format!("leveler-desktop-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config_dir = dir.join("configs");
        std::fs::create_dir_all(config_dir.join("providers")).unwrap();
        std::fs::create_dir_all(config_dir.join("models")).unwrap();
        std::fs::write(
            config_dir.join("providers/mock.yaml"),
            "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n",
        )
        .unwrap();
        std::fs::write(
            config_dir.join("models/m.yaml"),
            r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities: { streaming: true, tool_calling: true, parallel_tool_calls: false, structured_output: true, reasoning: false, vision: false }
limits: { context_window: 131072, reliable_context: 65536, max_output_tokens: 1024, max_tool_schema_bytes: 8192, max_parallel_tool_calls: 1 }
compatibility: { synthesize_tool_call_ids: true, drop_unsupported_fields: true }
"#,
        )
        .unwrap();
        let layout = Layout::no_workspace(
            leveler_core::LevelerHome::from_root(dir.join("home")),
            Some(config_dir),
        );
        let payload = model_list_payload(layout).unwrap();
        assert!(
            payload["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["provider"] == "mock" && model["model"] == "m"),
            "the configured model must be listed: {payload}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uncertain_delivery_retains_error_semantics_and_invalid_workspace_fails() {
        let error = anyhow::Error::new(leveler_client_protocol::ClientError::OutcomeUnknown(
            "lost response".into(),
        ));
        assert_eq!(error_kind(&error), "outcome_unknown");
        assert!(workspace_param(&json!({"workspace":42})).is_err());
        assert!(
            workspace_param(&json!({"workspace":null}))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn desktop_product_commands_keep_selected_session_scope() {
        let session = SessionId::new("desktop-session");
        for id in [session.clone(), SessionId::new("other-session")] {
            let commands = [
                ClientCommand::SelectModel {
                    session_id: id.clone(),
                    model: leveler_model::ModelRef::new("fixture", "model"),
                },
                ClientCommand::RenameSession {
                    session_id: id.clone(),
                    name: "Renamed task".into(),
                },
                ClientCommand::ArchiveSession {
                    session_id: id.clone(),
                },
                ClientCommand::UnarchiveSession {
                    session_id: id.clone(),
                },
            ];
            for command in commands {
                assert_eq!(validate_command(&session, &command).is_ok(), id == session);
            }
        }
        assert!(
            validate_command(
                &session,
                &ClientCommand::SetDefaultModel {
                    session_id: session.clone(),
                    model: leveler_model::ModelRef::new("fixture", "model"),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn desktop_settings_reads_keep_selected_session_and_bounded_query_scope() {
        let session = SessionId::new("desktop-settings");
        let id = Some("query-1".into());
        let commands = [
            ClientCommand::ListAgents {
                session_id: session.clone(),
                query_id: id.clone(),
            },
            ClientCommand::GetAgent {
                session_id: session.clone(),
                name: "reviewer".into(),
                query_id: id.clone(),
            },
            ClientCommand::QueryContext {
                session_id: session.clone(),
                query_id: id.clone(),
            },
            ClientCommand::QueryObservability {
                session_id: session.clone(),
                query_id: id.clone(),
                center_seq: None,
                before: 0,
                after: 100,
            },
            ClientCommand::QueryUsage {
                session_id: session.clone(),
                query_id: id.clone(),
                scope: leveler_client_protocol::UiUsageScope::Session,
                utc_offset_minutes: 480,
            },
        ];
        for command in commands {
            assert!(validate_command(&session, &command).is_ok());
            assert!(validate_command(&SessionId::new("other"), &command).is_err());
        }
        for id in [
            None,
            Some("".into()),
            Some(" ".into()),
            Some("a".repeat(257).into()),
        ] {
            assert!(
                validate_command(
                    &session,
                    &ClientCommand::ListAgents {
                        session_id: session.clone(),
                        query_id: id
                    }
                )
                .is_err()
            );
        }
        for name in ["Upper", "../agent", "a-", "con"] {
            assert!(
                validate_command(
                    &session,
                    &ClientCommand::GetAgent {
                        session_id: session.clone(),
                        query_id: Some("q".into()),
                        name: name.into()
                    }
                )
                .is_err()
            );
        }
        for (center_seq, before, after) in [
            (Some(-1), 0, 0),
            (Some(9007199254740992), 0, 0),
            (None, 101, 0),
            (None, 0, 101),
        ] {
            assert!(
                validate_command(
                    &session,
                    &ClientCommand::QueryObservability {
                        session_id: session.clone(),
                        query_id: Some("q".into()),
                        center_seq,
                        before,
                        after
                    }
                )
                .is_err()
            );
        }
    }

    #[test]
    fn desktop_skill_reads_are_session_scoped_and_bounded() {
        let session = SessionId::new("desktop-skills");
        let list = ClientCommand::ListSkills {
            session_id: session.clone(),
            query_id: Some("q".into()),
        };
        assert!(validate_command(&session, &list).is_ok());
        assert!(validate_command(&SessionId::new("other"), &list).is_err());

        for name in ["deploy", "pdf2", "my_skill", "a.b-c_1"] {
            let get = ClientCommand::GetSkill {
                session_id: session.clone(),
                name: name.into(),
                query_id: Some("q".into()),
            };
            assert!(validate_command(&session, &get).is_ok(), "{name}");
            assert!(validate_command(&SessionId::new("other"), &get).is_err());
        }
        for name in ["", " ", "../escape", ".hidden", "a/b", &"a".repeat(65)] {
            assert!(
                validate_command(
                    &session,
                    &ClientCommand::GetSkill {
                        session_id: session.clone(),
                        name: (*name).to_string(),
                        query_id: Some("q".into()),
                    }
                )
                .is_err(),
                "{name}"
            );
        }
        for id in [
            None,
            Some("".into()),
            Some(" ".into()),
            Some("a".repeat(257).into()),
        ] {
            assert!(
                validate_command(
                    &session,
                    &ClientCommand::ListSkills {
                        session_id: session.clone(),
                        query_id: id
                    }
                )
                .is_err()
            );
        }
    }

    #[test]
    fn desktop_background_cancel_is_scoped_and_bounded() {
        let session = SessionId::new("desktop-background");
        let cancel = ClientCommand::CancelBackgroundTask {
            session_id: session.clone(),
            task_id: "host-1".into(),
        };
        assert!(validate_command(&session, &cancel).is_ok());
        assert!(validate_command(&SessionId::new("other"), &cancel).is_err());
        for task_id in ["", " ", &"a".repeat(257)] {
            assert!(
                validate_command(
                    &session,
                    &ClientCommand::CancelBackgroundTask {
                        session_id: session.clone(),
                        task_id: (*task_id).to_string(),
                    }
                )
                .is_err()
            );
        }
    }

    #[test]
    fn desktop_diff_query_keeps_selected_session_scope() {
        let session = SessionId::new("desktop-diff");
        for query_id in [None, Some("query-1".into())] {
            let command = ClientCommand::RequestDiff {
                session_id: session.clone(),
                query_id,
            };
            assert!(validate_command(&session, &command).is_ok());
            assert!(validate_command(&SessionId::new("other"), &command).is_err());
        }
        for id in ["".to_string(), " ".to_string(), "a".repeat(257)] {
            let command = ClientCommand::RequestDiff {
                session_id: session.clone(),
                query_id: Some(id.into()),
            };
            assert!(validate_command(&session, &command).is_err());
        }
    }

    #[test]
    fn desktop_permission_selection_is_existing_typed_session_mode_only() {
        use leveler_client_protocol::PermissionProfile;
        let session = SessionId::new("permission-session");
        for mode in [
            PermissionProfile::FullAccess,
            PermissionProfile::Assisted,
            PermissionProfile::RequestApproval,
        ] {
            let command = ClientCommand::SetPermissionProfile {
                session_id: session.clone(),
                mode,
                contract: None,
            };
            assert!(validate_command(&session, &command).is_ok());
            assert!(validate_command(&SessionId::new("other"), &command).is_err());
        }
        for mode in [
            "full",
            "auto",
            "restricted",
            "auto_approve",
            "FullAccess",
            "",
        ] {
            assert!(
                serde_json::from_value::<ClientCommand>(
                    json!({"type":"set_permission_profile","session_id":session,"mode":mode})
                )
                .is_err()
            );
        }
    }

    /// R2b: the Desktop's mode picker writes the SAME typed axis command the
    /// terminal does, bound to the selected session. Values the runtime cannot
    /// represent are refused here; anything else stays the runtime's decision.
    #[test]
    fn desktop_collaboration_switch_is_the_existing_typed_axis_command() {
        let session = SessionId::new("mode-session");
        for collaboration in ["chat", "plan", "goal"] {
            let command = ClientCommand::SetProductAxes {
                session_id: session.clone(),
                work_profile: "single".to_string(),
                collaboration: collaboration.to_string(),
            };
            assert!(validate_command(&session, &command).is_ok());
            assert!(validate_command(&SessionId::new("other"), &command).is_err());
        }
        for collaboration in ["Chat", "Goal", "plan ", "", "auto", "orchestrate"] {
            let command = ClientCommand::SetProductAxes {
                session_id: session.clone(),
                work_profile: "single".to_string(),
                collaboration: collaboration.to_string(),
            };
            assert!(
                validate_command(&session, &command).is_err(),
                "`{collaboration}` must not be representable as an axis"
            );
        }
        let long_profile = ClientCommand::SetProductAxes {
            session_id: session.clone(),
            work_profile: "x".repeat(65),
            collaboration: "chat".to_string(),
        };
        assert!(validate_command(&session, &long_profile).is_err());
    }

    /// DESKTOP-SESSION-1: an ordinary Desktop task is a CONVERSATION.
    ///
    /// The bridge used to fill an omitted axis with `CollaborationMode::default()`
    /// (the coding-session default, Goal), so a task the reader started from a
    /// UI that said `chat` ran the goal lifecycle and ended
    /// `goal_unresolved`.
    #[test]
    fn a_desktop_task_without_a_stated_axis_is_a_conversation() {
        use leveler_local_transport::CollaborationMode;
        assert_eq!(
            requested_collaboration(&json!({"goal": "new task"})).unwrap(),
            CollaborationMode::Chat
        );
        assert_eq!(
            requested_collaboration(&json!({"goal": "new task", "collaboration": null})).unwrap(),
            CollaborationMode::Chat
        );
        // A stated axis is the client's request, not this host's default.
        assert_eq!(
            requested_collaboration(&json!({"goal": "g", "collaboration": "goal"})).unwrap(),
            CollaborationMode::Goal
        );
        assert_eq!(
            requested_collaboration(&json!({"goal": "g", "collaboration": "plan"})).unwrap(),
            CollaborationMode::Plan
        );
        // And it is NOT the coding-session default, so the two cannot drift.
        assert_ne!(
            CollaborationMode::interactive_session(),
            CollaborationMode::default()
        );
    }

    #[test]
    fn desktop_schedule_management_is_session_scoped_and_bounded() {
        use leveler_client_protocol::schedule::{
            ScheduleActionWire, ScheduleDraft, SchedulePatch, ScheduleRequest, ScheduleTriggerWire,
        };
        let session = SessionId::new("desktop-schedule");
        let command = |requester: &SessionId, query_id: &str, request: ScheduleRequest| {
            ClientCommand::ManageSchedule {
                requester_session_id: requester.clone(),
                query_id: query_id.into(),
                request: Box::new(request),
            }
        };
        let draft = ScheduleDraft {
            name: "morning report".into(),
            workspace: "/repo".into(),
            action: ScheduleActionWire::Agent {
                goal: "summarize the night".into(),
            },
            trigger: ScheduleTriggerWire::Daily {
                time: "09:00".into(),
                timezone: Some("Asia/Shanghai".into()),
            },
            session_policy: None,
            overlap: None,
            mode: Some("assisted".into()),
            model: None,
            timeout_secs: None,
            misfire: None,
            misfire_grace_secs: None,
            grant_unattended: false,
            allow_external_side_effects: false,
        };
        for request in [
            ScheduleRequest::List { filter: None },
            ScheduleRequest::Status,
            ScheduleRequest::WakeupDoctor,
            ScheduleRequest::Show {
                id: "schedule-1".into(),
            },
            ScheduleRequest::Create {
                draft: draft.clone(),
            },
            ScheduleRequest::Update {
                id: "schedule-1".into(),
                expected_revision: Some("rev-1".into()),
                patch: SchedulePatch {
                    name: Some("renamed".into()),
                    // A one-off retime must reach the same manager; the
                    // bridge validates shape and leaves the store decision
                    // to the schedule domain.
                    trigger: Some(ScheduleTriggerWire::Once {
                        at: "2026-11-06T10:30:00Z".into(),
                    }),
                    ..SchedulePatch::default()
                },
            },
            ScheduleRequest::Runs {
                definition: Some("schedule-1".into()),
                limit: Some(50),
            },
        ] {
            assert!(
                validate_command(&session, &command(&session, "query-1", request.clone())).is_ok()
            );
            assert!(
                validate_command(
                    &SessionId::new("other-session"),
                    &command(&session, "query-1", request.clone())
                )
                .is_err()
            );
        }
        for query_id in ["", " ", &"a".repeat(257)] {
            assert!(
                validate_command(
                    &session,
                    &command(&session, query_id, ScheduleRequest::Status)
                )
                .is_err()
            );
        }
        for id in [String::new(), " ".into(), "a".repeat(257)] {
            assert!(
                validate_command(
                    &session,
                    &command(&session, "query-1", ScheduleRequest::Show { id })
                )
                .is_err()
            );
        }
        assert!(
            validate_command(
                &session,
                &command(
                    &session,
                    "query-1",
                    ScheduleRequest::Runs {
                        definition: None,
                        limit: Some(501),
                    }
                )
            )
            .is_err()
        );
        let too_long = "a".repeat(100_001);
        for action in [
            ScheduleActionWire::Agent {
                goal: too_long.clone(),
            },
            ScheduleActionWire::Shell {
                command: too_long.clone(),
            },
            ScheduleActionWire::Native {
                action: "open".into(),
                input: too_long.clone(),
            },
        ] {
            let mut bad = draft.clone();
            bad.action = action;
            assert!(
                validate_command(
                    &session,
                    &command(&session, "query-1", ScheduleRequest::Create { draft: bad })
                )
                .is_err()
            );
        }
    }

    #[test]
    fn desktop_commands_cannot_shutdown_runtime_or_cross_sessions() {
        let session = leveler_core::SessionId::new("desktop-session");
        assert!(validate_command(&session, &leveler_client_protocol::ClientCommand::Quit).is_err());
        assert!(
            validate_command(
                &session,
                &leveler_client_protocol::ClientCommand::SubmitMessage {
                    session_id: leveler_core::SessionId::new("other"),
                    content: "hello".into(),
                    attachments: vec![],
                }
            )
            .is_err()
        );
        assert!(
            validate_command(
                &session,
                &leveler_client_protocol::ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "hello".into(),
                    attachments: vec![],
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn desktop_upload_and_image_submission_keep_selected_session_scope() {
        use leveler_client_protocol::{AttachmentId, AttachmentKind, AttachmentRef};
        let session = SessionId::new("desktop-upload");
        let upload = ClientCommand::AddAttachmentData {
            session_id: session.clone(),
            name: "note.txt".into(),
            data_base64: "aGVsbG8=".into(),
        };
        assert!(validate_command(&session, &upload).is_ok());
        assert!(validate_command(&SessionId::new("other"), &upload).is_err());
        let image = AttachmentRef {
            id: AttachmentId::new("image"),
            kind: AttachmentKind::Image,
            name: "image.png".into(),
            mime_type: "image/png".into(),
            size_bytes: 20,
            sha256: "a".repeat(64),
            width: Some(4),
            height: Some(3),
            source_path: None,
            preview_path: None,
        };
        assert!(
            validate_command(
                &session,
                &ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: String::new(),
                    attachments: vec![image.clone()],
                }
            )
            .is_ok()
        );
        assert!(
            validate_command(
                &SessionId::new("other"),
                &ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "read".into(),
                    attachments: vec![image.clone()],
                }
            )
            .is_err()
        );
        let mut unsupported = image.clone();
        unsupported.kind = AttachmentKind::TextFile;
        assert!(
            validate_command(
                &session,
                &ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "read".into(),
                    attachments: vec![unsupported],
                }
            )
            .is_err()
        );
        let mut forged = image;
        forged.sha256 = "../secret".into();
        assert!(
            validate_command(
                &session,
                &ClientCommand::SubmitMessage {
                    session_id: session.clone(),
                    content: "read".into(),
                    attachments: vec![forged],
                }
            )
            .is_err()
        );
        assert!(
            validate_command(
                &session,
                &ClientCommand::AddAttachment {
                    session_id: session.clone(),
                    path: "/arbitrary/path".into(),
                    name: None,
                }
            )
            .is_err()
        );
    }

    /// MCP management is the existing runtime command; the desktop adapter
    /// only binds it to the selected session and bounds the declaration.
    #[test]
    fn desktop_mcp_management_is_session_scoped_and_bounded() {
        let session = SessionId::new("desktop-mcp");
        let command = |requester: &SessionId, request: McpRequest| ClientCommand::ManageMcp {
            requester_session_id: requester.clone(),
            query_id: leveler_core::CommandId::new("mcp-q1"),
            request: Box::new(request),
        };
        let stdio = McpServerSpec {
            name: "echo".into(),
            transport: McpTransport::Stdio,
            command: Some("/usr/bin/python3".into()),
            args: vec!["-u".into(), "/tmp/echo.py".into()],
            env: vec![leveler_client_protocol::mcp::McpNameRef {
                key: "API_KEY".into(),
                source: "API_KEY".into(),
            }],
            url: None,
            headers: Vec::new(),
            bearer_token_env: None,
            enabled: true,
        };
        assert!(
            validate_command(
                &session,
                &command(
                    &session,
                    McpRequest::Add {
                        spec: stdio.clone()
                    }
                )
            )
            .is_ok()
        );
        assert!(
            validate_command(
                &session,
                &command(
                    &SessionId::new("other"),
                    McpRequest::Add {
                        spec: stdio.clone()
                    }
                )
            )
            .is_err(),
            "the request must name the envelope's session"
        );
        for request in [McpRequest::List, McpRequest::Reload] {
            assert!(validate_command(&session, &command(&session, request)).is_ok());
        }
        for server in ["", "../escape", "bad name", &"a".repeat(65)] {
            assert!(
                validate_command(
                    &session,
                    &command(
                        &session,
                        McpRequest::Reconnect {
                            server: server.into()
                        }
                    )
                )
                .is_err()
            );
        }
        let mut mixed = stdio.clone();
        mixed.url = Some("https://example.com/mcp".into());
        assert!(
            validate_command(
                &session,
                &command(&session, McpRequest::Add { spec: mixed })
            )
            .is_err()
        );
        let mut missing = stdio.clone();
        missing.command = None;
        assert!(
            validate_command(
                &session,
                &command(&session, McpRequest::Add { spec: missing })
            )
            .is_err()
        );
        let http = McpServerSpec {
            name: "remote".into(),
            transport: McpTransport::StreamableHttp,
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            url: Some("https://example.com/mcp".into()),
            headers: vec![leveler_client_protocol::mcp::McpNameRef {
                key: "X-API-Key".into(),
                source: "SERVICE_API_KEY".into(),
            }],
            bearer_token_env: Some("SERVICE_TOKEN".into()),
            enabled: true,
        };
        assert!(
            validate_command(
                &session,
                &command(&session, McpRequest::Add { spec: http.clone() })
            )
            .is_ok()
        );
        let mut credentialed = http.clone();
        credentialed.url = Some("https://user:secret@example.com/mcp".into());
        assert!(
            validate_command(
                &session,
                &command(&session, McpRequest::Add { spec: credentialed })
            )
            .is_err()
        );
        let mut with_command = http;
        with_command.command = Some("npx".into());
        assert!(
            validate_command(
                &session,
                &command(&session, McpRequest::Add { spec: with_command })
            )
            .is_err()
        );
    }
}
