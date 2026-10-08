//! JSONL desktop adapter; runtime host owns daemon lifetime and session truth.
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
        let _ = self
            .0
            .try_send(json!({"event": "handoff", "data": event}));
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
        ClientCommand::ListMemory { query_id, .. }
        | ClientCommand::ListAgents { query_id, .. }
        | ClientCommand::QueryContext { query_id, .. }
            if valid_desktop_query_id(query_id) => {}
        ClientCommand::GetAgent { query_id, name, .. }
            if valid_desktop_query_id(query_id)
                && leveler_agent::agent_registry::validate_agent_name(name).is_ok() => {}
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
        ClientCommand::SteerCurrentTurn { .. }
        | ClientCommand::CancelCurrentTurn { .. }
        | ClientCommand::ForceCancelCurrentTurn { .. }
        | ClientCommand::CancelTask { .. }
        | ClientCommand::QuerySessionHistory { .. }
        | ClientCommand::SelectModel { .. }
        | ClientCommand::SetPermissionProfile { .. }
        | ClientCommand::RenameSession { .. }
        | ClientCommand::ArchiveSession { .. }
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
    id.as_ref()
        .is_some_and(|id| !id.as_str().trim().is_empty() && id.as_str().len() <= 256)
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

struct Bridge {
    home: leveler_core::LevelerHome,
    config_dir: Option<PathBuf>,
    client: Option<Arc<LocalSocketRuntimeClient>>,
    layout: Option<Layout>,
    selected: Option<SessionId>,
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
                    Ok(event) => json!({"event":"runtime", "session_id":session, "data":event}),
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
    async fn handle(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        match method {
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
            "create" => {
                let client = self.client()?;
                let workspace = self
                    .layout
                    .as_ref()
                    .and_then(|layout| layout.primary_workspace());
                let request = CreateSessionRequest {
                    collaboration: match params.get("collaboration") {
                        Some(value) if !value.is_null() => serde_json::from_value(value.clone())?,
                        // Desktop has no axis selector yet: a new session gets
                        // the product default, the same one the daemon
                        // resolves for a request that omits the field.
                        _ => leveler_local_transport::CollaborationMode::default(),
                    },
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
            "runtime_info" => Ok(serde_json::to_value(
                LocalRuntimeService::runtime_info(self.client()?.as_ref()).await?,
            )?),
            "deliver" => {
                let envelope: CommandEnvelope = serde_json::from_value(params["envelope"].clone())?;
                anyhow::ensure!(
                    self.selected.as_ref() == Some(&envelope.session_id),
                    "session is not selected"
                );
                validate_command(&envelope.session_id, &envelope.command)?;
                self.client()?.deliver(envelope).await?;
                Ok(Value::Null)
            }
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
        subscription: None,
        output: output.clone(),
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
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
fn lifecycle_state(
    error: &anyhow::Error,
) -> Option<leveler_runtime_host::RuntimeLifecycleState> {
    error
        .downcast_ref::<leveler_runtime_host::EnsureError>()
        .map(leveler_runtime_host::RuntimeLifecycleState::from_ensure_error)
}

/// The structured error a Desktop client receives.
///
/// `kind` is the protocol error class; `state` and `next_step` are present only
/// for a lifecycle failure, and they come from the SAME vocabulary the terminal
/// and the Web use — so a Desktop client can offer the actionable next step
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
        Some(ClientError::Unresolvable(_)) => "unresolvable",
        Some(ClientError::OwnershipConflict(_)) => "ownership_conflict",
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
#[cfg(test)]
mod tests {
    use super::*;

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
    fn desktop_memory_listing_is_scoped_correlated_and_not_mutating() {
        let session = SessionId::new("desktop-memory");
        for include_archived in [false, true] {
            let command = ClientCommand::ListMemory {
                session_id: session.clone(),
                query_id: Some("query".into()),
                include_archived,
            };
            assert!(validate_command(&session, &command).is_ok());
            assert!(validate_command(&SessionId::new("other"), &command).is_err());
        }
        for query_id in [None, Some(" ".into()), Some("a".repeat(257).into())] {
            assert!(
                validate_command(
                    &session,
                    &ClientCommand::ListMemory {
                        session_id: session.clone(),
                        query_id,
                        include_archived: false
                    }
                )
                .is_err()
            );
        }
        assert!(
            validate_command(
                &session,
                &ClientCommand::ForgetMemory {
                    session_id: session.clone(),
                    id: "memory".into()
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
}
