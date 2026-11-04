//! Deterministic native actions over installed system applications.
//!
//! Three thin adapters over [`leveler_native::NativeActions`]:
//! `native_capabilities` probes what this machine can actually do,
//! `native_action` performs a mutating action, and `native_verify` reads the
//! result a send left behind. Looking, acting and verifying have different
//! risk and different permissions, so they are not folded into one tool whose
//! static risk would have to lie about one of them.
//!
//! No tool carries a plan, a retry loop or a second approval decision. The
//! runtime admits the call; the host execution authority starts the process;
//! the crate interprets the process facts.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{ProcessRequest, RiskLevel};
use leveler_native::{
    ActionId, ActionStatus, ExecutionEvidence, NativeActionRequest, NativeActions,
};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiscoverInput {
    /// Optional capability id to filter, such as `mail`. Omit for all.
    #[serde(default)]
    capability: Option<String>,
}

pub struct NativeCapabilitiesTool {
    native: std::sync::Arc<NativeActions>,
}

impl NativeCapabilitiesTool {
    pub fn new(native: std::sync::Arc<NativeActions>) -> Self {
        Self { native }
    }
}

#[async_trait]
impl Tool for NativeCapabilitiesTool {
    fn name(&self) -> &'static str {
        "native_capabilities"
    }

    fn description(&self) -> &'static str {
        "Probe this machine's deterministic native actions: which applications \
         are installed, whether the OS grants automation permission, whether an \
         account can send, and what each action can and cannot prove. Discovery \
         does not launch applications, does not ask for permission and does not \
         send anything. Call this before native_action. When it reports an \
         action supported, prefer that deterministic action over pixel-level \
         Computer Use for the same task."
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<DiscoverInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn replay_is_side_effect_free(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: DiscoverInput = super::parse_input(self.name(), input)?;
        let report = self.native.discover(&context.execution.runner).await;
        if let Some(filter) = input.capability.as_deref() {
            let exists = report
                .capabilities
                .iter()
                .any(|capability| capability.id == filter);
            if !exists {
                let ids: Vec<_> = report
                    .capabilities
                    .iter()
                    .map(|capability| capability.id)
                    .collect();
                return Err(ToolError::InvalidArguments {
                    tool: self.name().to_string(),
                    message: format!("unknown capability {filter:?}; this host reported {ids:?}"),
                });
            }
        }
        let mut report = report;
        if let Some(filter) = input.capability.as_deref() {
            report
                .capabilities
                .retain(|capability| capability.id == filter);
        }
        let metadata = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
        let content = serde_json::to_string_pretty(&report)
            .unwrap_or_else(|_| "native capability discovery could not be rendered".to_string());
        Ok(ToolOutput::ok(content).with_metadata(metadata))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ActionInput {
    /// One deterministic action. Sending has an external side effect.
    action: ActionId,
    /// One plain recipient address; no display name and no list.
    to: String,
    /// Exact subject; line breaks are rejected.
    subject: String,
    /// Message body. Required for draft/open/send, rejected for a Sent check.
    #[serde(default)]
    body: Option<String>,
}

pub struct NativeActionTool {
    native: std::sync::Arc<NativeActions>,
}

impl NativeActionTool {
    pub fn new(native: std::sync::Arc<NativeActions>) -> Self {
        Self { native }
    }
}

#[async_trait]
impl Tool for NativeActionTool {
    fn name(&self) -> &'static str {
        "native_action"
    }

    fn description(&self) -> &'static str {
        "Run one deterministic mail action through the installed mail \
         application: mail.draft.create saves a draft, mail.send.open_ui opens \
         a compose window for a person to send, and mail.send.submit asks the \
         application to send. Use native_capabilities first. A submit reports \
         `submitted`, never delivered; `unknown` means the effect could not be \
         established and the action must not be retried automatically."
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ActionInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Destructive
    }

    /// Names the action, recipient and subject for the approval prompt. The
    /// body is deliberately absent: it can be large or private, and the
    /// approval is about who receives what kind of action, not its text.
    fn approval_reason(&self, input: &serde_json::Value, _context: &ToolContext) -> Option<String> {
        let input: ActionInput = serde_json::from_value(input.clone()).ok()?;
        let effect = match input.action {
            ActionId::MailDraftCreate => "save a draft in the user's mail application",
            ActionId::MailSendOpenUi => "open a compose window the user must send",
            ActionId::MailSendSubmit => "submit a message for delivery",
            ActionId::MailSentCheck => "read the Sent mailbox",
        };
        Some(format!(
            "{effect} via native action {}: recipient {}, subject {:?}",
            input.action.as_str(),
            input.to,
            input.subject
        ))
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: ActionInput = super::parse_input(self.name(), input)?;
        if input.action == ActionId::MailSentCheck {
            return Err(ToolError::InvalidArguments {
                tool: self.name().to_string(),
                message: "mail.sent.check is read-only; use native_verify".to_string(),
            });
        }
        let Some(body) = input.body else {
            return Err(ToolError::InvalidArguments {
                tool: self.name().to_string(),
                message: "body is required for this action; pass an empty string for none"
                    .to_string(),
            });
        };
        run_action(
            &self.native,
            input.action,
            input.to,
            input.subject,
            Some(body),
            context,
            cancellation,
        )
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct VerifyInput {
    /// One plain recipient address.
    to: String,
    /// Exact subject the sent message must have.
    subject: String,
}

pub struct NativeVerifyTool {
    native: std::sync::Arc<NativeActions>,
}

impl NativeVerifyTool {
    pub fn new(native: std::sync::Arc<NativeActions>) -> Self {
        Self { native }
    }
}

#[async_trait]
impl Tool for NativeVerifyTool {
    fn name(&self) -> &'static str {
        "native_verify"
    }

    fn description(&self) -> &'static str {
        "Check whether a message to this recipient with this exact subject \
         appears in the Sent mailbox. A match is a Sent record, not a delivery \
         receipt; a miss is only the result of a bounded scan and is not proof \
         that nothing was sent. Read-only."
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<VerifyInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn replay_is_side_effect_free(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: VerifyInput = super::parse_input(self.name(), input)?;
        run_action(
            &self.native,
            ActionId::MailSentCheck,
            input.to,
            input.subject,
            None,
            context,
            cancellation,
        )
        .await
    }
}

async fn run_action(
    native: &NativeActions,
    action: ActionId,
    to: String,
    subject: String,
    body: Option<String>,
    context: ToolContext,
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolError> {
    // Mail performs its own network I/O outside any sandbox this process
    // controls. A denial we cannot enforce on the target application is a
    // refusal, not a warning.
    if context.policy.network_denied() {
        return Ok(ToolOutput::error(format!(
            "native action {} was not run: this execution's network scope is {:?}, and the \
             target mail application performs network I/O outside the sandbox CodeLeveler can \
             enforce.",
            action.as_str(),
            context.policy.network_scope()
        )));
    }

    let input = match body {
        Some(body) => serde_json::json!({
            "to": to,
            "subject": subject,
            "body": body,
        }),
        None => serde_json::json!({ "to": to, "subject": subject }),
    };
    let request = NativeActionRequest { action, input };
    let plan = match native.plan(&request) {
        Ok(plan) => plan,
        Err(error) => return Ok(ToolOutput::error(error.to_string())),
    };

    // Re-probe on every call: a user may have granted automation, installed a
    // client or signed out of an account since the last check.
    let report = native.discover(&context.execution.runner).await;
    let provider = plan.provider().as_str();
    let Some(capability) = report
        .capabilities
        .iter()
        .find(|capability| capability.provider == provider)
    else {
        return Ok(ToolOutput::error(format!(
            "native action {} was not run: the discovery probe did not report provider {provider}",
            action.as_str()
        )));
    };
    let action_report = capability
        .actions
        .iter()
        .find(|candidate| candidate.id == action.as_str());
    if !action_report.is_some_and(|report| report.supported) {
        let reason = action_report
            .and_then(|report| report.reason.as_deref())
            .or(capability.reason.as_deref())
            .unwrap_or("no supported capability was reported");
        return Ok(ToolOutput::error(format!(
            "native action {} is not supported here: {reason}",
            action.as_str()
        )));
    }
    if let Some(reason) = blocking_reason(capability) {
        return Ok(ToolOutput::error(format!(
            "native action {} was not run: {reason}",
            action.as_str()
        )));
    }
    if capability.permission == leveler_native::PermissionState::NotDetermined {
        // The action itself asks the OS now; if nobody answers, the timeout
        // produces `unknown` and the action is never retried.
        tracing::info!(
            provider,
            action = action.as_str(),
            "native action will request automation consent"
        );
    }

    let mut process = ProcessRequest::new(
        plan.program(),
        plan.args().to_vec(),
        plan.cwd().to_path_buf(),
    );
    process.timeout = plan.timeout();
    process.max_output_bytes = 256 * 1024;
    let started = std::time::Instant::now();
    let output = context.execution.runner.run(process, cancellation).await;
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let evidence = match output {
        Ok(output) => ExecutionEvidence {
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            stdout: output.stdout,
            stderr: output.stderr,
            duration_ms,
        },
        Err(error) => ExecutionEvidence {
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: error.to_string(),
            duration_ms,
        },
    };
    let outcome = native.interpret(action, plan.provider(), &evidence);
    let metadata = serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null);
    let content = serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| {
        format!(
            "native action {} completed with an unrenderable outcome",
            action.as_str()
        )
    });
    let is_error = !matches!(
        outcome.status,
        ActionStatus::Completed | ActionStatus::Submitted
    );
    Ok(ToolOutput {
        content: if outcome.status == ActionStatus::Unknown {
            format!(
                "{content}\n\nThe effect is unknown. Do not retry this action automatically; \
                 verify the Sent state or ask the user."
            )
        } else {
            content
        },
        is_error,
        metadata,
    })
}

/// Facts that make execution pointless before a process is started. An
/// unobserved account configuration is not one of them: Mail may be closed,
/// and the action itself can launch it. An unknown *permission* is one.
fn blocking_reason(capability: &leveler_native::CapabilityReport) -> Option<String> {
    use leveler_native::CapabilityState;
    match capability.state {
        CapabilityState::Unavailable | CapabilityState::Unsupported => Some(
            capability
                .reason
                .clone()
                .unwrap_or_else(|| "the capability is not available".to_string()),
        ),
        CapabilityState::RequiresPermission
            if capability.permission == leveler_native::PermissionState::NotDetermined =>
        {
            None
        }
        CapabilityState::RequiresPermission => Some(
            capability
                .reason
                .clone()
                .unwrap_or_else(|| "automation permission is required".to_string()),
        ),
        CapabilityState::RequiresConfiguration => Some(
            capability
                .reason
                .clone()
                .unwrap_or_else(|| "an account must be configured first".to_string()),
        ),
        // `Unknown` from unobserved account configuration is safe to attempt:
        // the action reports the missing account itself. An unknown
        // *permission* is not: the probe could not establish whether the OS
        // will allow the action, so starting it would guess or hang again.
        CapabilityState::Unknown
            if capability.permission == leveler_native::PermissionState::Unknown =>
        {
            Some(
                capability
                    .reason
                    .clone()
                    .unwrap_or_else(|| "automation permission could not be determined".to_string()),
            )
        }
        CapabilityState::Installed | CapabilityState::Available | CapabilityState::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{CapabilityPacks, model_surface};
    use leveler_execution::PermissionProfile;
    use std::sync::Arc;

    fn native() -> Arc<NativeActions> {
        Arc::new(NativeActions::new())
    }

    fn names(registry: &crate::ToolRegistry) -> Vec<String> {
        registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect()
    }

    #[test]
    fn native_tools_need_both_the_pack_and_the_handle() {
        let capabilities = crate::tools::test_capabilities().with_native_actions(native());
        let packs = CapabilityPacks {
            native_actions: true,
            ..CapabilityPacks::NONE
        };
        let exposed = names(&model_surface(packs, &capabilities));
        assert!(exposed.iter().any(|name| name == "native_capabilities"));
        assert!(exposed.iter().any(|name| name == "native_action"));
        assert!(exposed.iter().any(|name| name == "native_verify"));

        // The flag alone, with no backend, is the same mechanical fact as no
        // capability: no tool is built that could only fail.
        let without = names(&model_surface(packs, &crate::tools::test_capabilities()));
        assert!(!without.iter().any(|name| name.starts_with("native_")));

        let off = names(&model_surface(CapabilityPacks::NONE, &capabilities));
        assert!(!off.iter().any(|name| name.starts_with("native_")));
    }

    #[test]
    fn discovering_and_verifying_are_safe_while_acting_is_destructive() {
        assert_eq!(
            NativeCapabilitiesTool::new(native()).risk(),
            RiskLevel::Safe
        );
        assert_eq!(NativeVerifyTool::new(native()).risk(), RiskLevel::Safe);
        assert_eq!(
            NativeActionTool::new(native()).risk(),
            RiskLevel::Destructive
        );
    }

    #[test]
    fn an_unknown_permission_blocks_while_unknown_configuration_does_not() {
        let mut capability = leveler_native::CapabilityReport {
            id: "mail",
            provider: "macos.mail",
            provider_name: "Apple Mail",
            state: leveler_native::CapabilityState::Unknown,
            installed: true,
            permission: leveler_native::PermissionState::Unknown,
            requires_system_authorization: true,
            requires_interactive_session: true,
            configured: None,
            reason: Some("the permission probe timed out".into()),
            evidence: Vec::new(),
            actions: Vec::new(),
        };
        assert_eq!(
            blocking_reason(&capability).as_deref(),
            Some("the permission probe timed out")
        );

        capability.permission = leveler_native::PermissionState::Granted;
        capability.reason = Some("Mail is not running".into());
        assert!(blocking_reason(&capability).is_none());
    }

    #[test]
    fn the_action_schema_names_every_action() {
        let schema = NativeActionTool::new(native()).input_schema();
        let action = &schema["properties"]["action"];
        let values = action
            .get("enum")
            .or_else(|| action["allOf"][0].get("enum"))
            .and_then(|values| values.as_array())
            .unwrap_or_else(|| panic!("action enum missing from {schema}"));
        for action in ActionId::ALL {
            assert!(
                values.iter().any(|value| value == action.as_str()),
                "{} missing from {schema}",
                action.as_str()
            );
        }
        assert!(
            schema["properties"]["body"]["type"]
                .as_array()
                .is_some_and(|types| types.iter().any(|kind| kind == "string")),
            "body must accept a string: {schema}"
        );
    }

    #[test]
    fn an_approval_prompt_names_recipient_and_subject_but_never_the_body() {
        let tool = NativeActionTool::new(native());
        let (context, _dir) = crate::tools::test_ctx(PermissionProfile::Assisted, &[]);
        let reason = tool
            .approval_reason(
                &serde_json::json!({
                    "action": "mail.send.submit",
                    "to": "controlled@example.test",
                    "subject": "weekly report",
                    "body": "correct horse battery staple",
                }),
                &context,
            )
            .expect("sending always explains itself");
        assert!(reason.contains("submit"), "{reason}");
        assert!(reason.contains("controlled@example.test"), "{reason}");
        assert!(reason.contains("weekly report"), "{reason}");
        assert!(
            !reason.contains("correct"),
            "the prompt leaked the body: {reason}"
        );
    }

    #[tokio::test]
    async fn a_network_denial_refuses_before_any_process_or_application() {
        let tool = NativeActionTool::new(native());
        let (context, _dir) = crate::tools::test_ctx(PermissionProfile::Assisted, &[]);
        let output = tool
            .execute(
                serde_json::json!({
                    "action": "mail.send.submit",
                    "to": "controlled@example.test",
                    "subject": "denied",
                    "body": "must not leave the machine",
                }),
                context.with_sandbox(true),
                CancellationToken::new(),
            )
            .await
            .expect("a denial is a model-visible result, not an infrastructure error");
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("network scope"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn a_sent_check_is_routed_to_the_read_only_tool() {
        let tool = NativeActionTool::new(native());
        let (context, _dir) = crate::tools::test_ctx(PermissionProfile::Assisted, &[]);
        let error = tool
            .execute(
                serde_json::json!({
                    "action": "mail.sent.check",
                    "to": "controlled@example.test",
                    "subject": "check me",
                }),
                context,
                CancellationToken::new(),
            )
            .await
            .expect_err("native_action must not run a read-only check");
        assert!(error.to_string().contains("native_verify"), "{error}");
    }

    #[tokio::test]
    async fn a_message_action_requires_an_explicit_body() {
        let tool = NativeActionTool::new(native());
        let (context, _dir) = crate::tools::test_ctx(PermissionProfile::Assisted, &[]);
        let error = tool
            .execute(
                serde_json::json!({
                    "action": "mail.draft.create",
                    "to": "controlled@example.test",
                    "subject": "no body",
                }),
                context,
                CancellationToken::new(),
            )
            .await
            .expect_err("a message action without a body is invalid");
        assert!(error.to_string().contains("body is required"), "{error}");
    }
}
