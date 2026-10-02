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
        let _ = self
            .0
            .try_send(json!({"event":"handoff", "data":format!("{event:?}")}));
    }
    fn input(&self) -> Option<mpsc::UnboundedReceiver<HandoffAction>> {
        None
    }
}

fn validate_command(session: &SessionId, command: &ClientCommand) -> anyhow::Result<()> {
    match command {
        ClientCommand::SubmitMessage { attachments, .. } if attachments.is_empty() => {}
        ClientCommand::SteerCurrentTurn { .. }
        | ClientCommand::CancelCurrentTurn { .. }
        | ClientCommand::ForceCancelCurrentTurn { .. }
        | ClientCommand::CancelTask { .. }
        | ClientCommand::QuerySessionHistory { .. }
        | ClientCommand::ApprovalDecision { .. }
        | ClientCommand::AnswerClarification { .. } => {}
        _ => anyhow::bail!("command is outside Phase 3C desktop scope"),
    }
    if command.session_id().is_some_and(|id| id != session) {
        anyhow::bail!("command session does not match envelope");
    }
    Ok(())
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
                Err(error) => {
                    json!({"id":request.id,"ok":false,"error":{"message":error.to_string(),"kind":error_kind(&error)}})
                }
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
}
