//! Native capability discovery and deterministic local actions.
//!
//! This crate owns *what the machine can actually do* with its existing apps
//! and tools, and the prepared execution of one action. It does not own a
//! model loop, a tool registry, an approval decision or a schedule: the runtime
//! decides, the host execution authority performs, and this crate reports what
//! was observed.
//!
//! Discovery is a live probe on every call. It never launches an application
//! and never asks for an OS permission prompt; a fact that would require either
//! is reported as `unknown` with the exact reason.
//!
//! User content (recipient, subject, body) is written to private payload files
//! and read by a static script. It never enters a script's source or its
//! command line.

mod classify;
mod discover;
mod model;
mod protocol;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub use discover::{
    ActionReport, CapabilityReport, CapabilityState, DiscoveryReport, Evidence, PermissionMode,
    PermissionState, SystemToolReport, UnattendedSupport,
};
pub use model::{
    ActionId, ActionPlan, ActionStatus, DeliveryState, ExecutionEvidence, NativeActionRequest,
    NativeError, NativeOutcome, ProviderId, SentState,
};

/// One live admission-and-execution attempt.
///
/// `outcome.status == RequiresPermission` and the other non-executed statuses
/// carry an empty `execution_id`: no process was started. Callers must not
/// record an execution fact for those outcomes.
#[derive(Debug, Clone)]
pub struct NativeRun {
    pub provider: ProviderId,
    pub capability: Option<CapabilityReport>,
    pub outcome: NativeOutcome,
}

/// The capability handle injected into the tool surface and used directly by
/// the zero-model CLI path.
///
/// The handle owns the bounded TCC probe registry: one stuck Apple Event
/// call cannot permanently block later discovery on a fresh handle.
#[derive(Debug, Default, Clone)]
pub struct NativeActions {
    #[cfg(target_os = "macos")]
    probes: macos::PermissionProbes,
}

impl NativeActions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this binary has a native-action backend at all. Runtime facts
    /// (permissions, accounts) are reported by [`Self::discover`] on every call.
    pub const fn platform_supported() -> bool {
        cfg!(any(target_os = "macos", windows))
    }

    /// Live probe. Never launches an application and never prompts; every
    /// process it starts goes through the same host execution authority the
    /// actions use.
    pub async fn discover(&self, runner: &leveler_execution::CommandRunner) -> DiscoveryReport {
        #[cfg(target_os = "macos")]
        {
            macos::discover(runner, &self.probes).await
        }
        #[cfg(windows)]
        {
            let _ = runner;
            windows::discover()
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = runner;
            unsupported::discover()
        }
    }

    /// Validate the structured input and prepare every file and argument the
    /// execution needs. This performs no side effect.
    pub fn plan(&self, request: &NativeActionRequest) -> Result<ActionPlan, NativeError> {
        let payload = protocol::parse_input(request.action, &request.input)?;
        #[cfg(target_os = "macos")]
        {
            macos::plan(request.action, payload)
        }
        #[cfg(windows)]
        {
            windows::plan(request.action, payload)
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = payload;
            Err(NativeError::Unsupported(
                request.action.as_str(),
                format!("no native-action backend for {}", std::env::consts::OS),
            ))
        }
    }

    /// Interpret the process facts of a prepared action. The exit code alone is
    /// never treated as success.
    pub fn interpret(
        &self,
        action: ActionId,
        provider: ProviderId,
        evidence: &ExecutionEvidence,
    ) -> NativeOutcome {
        protocol::interpret(action, provider, evidence)
    }

    /// Plan, re-probe, admit and execute one action through the host execution
    /// authority. This is the single execution path for the native tool, the
    /// zero-model CLI and scheduled native actions.
    ///
    /// `timeout` may shorten the provider plan's own bound; it never extends
    /// it. Cancellation and process-tree termination stay in `CommandRunner`.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        runner: &leveler_execution::CommandRunner,
        request: &NativeActionRequest,
        mode: PermissionMode,
        timeout: Option<std::time::Duration>,
        max_output_bytes: usize,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<NativeRun, NativeError> {
        use crate::{ActionStatus, ExecutionEvidence};

        let plan = self.plan(request)?;
        let provider = plan.provider();
        // Re-probe on every attempt: permissions, installed clients and
        // signed-in accounts can change between creation and execution.
        let report = self.discover(runner).await;
        let capability = report
            .capabilities
            .iter()
            .find(|capability| capability.provider == provider.as_str())
            .cloned();

        let Some(capability) = capability else {
            let detail = format!(
                "the discovery probe did not report provider {}",
                provider.as_str()
            );
            return Ok(NativeRun {
                provider,
                capability: None,
                outcome: blocked_outcome(
                    request.action,
                    provider,
                    ActionStatus::Unsupported,
                    detail,
                ),
            });
        };

        if let Some((status, detail)) = capability.admission_block(request.action, mode) {
            return Ok(NativeRun {
                provider,
                capability: Some(capability),
                outcome: blocked_outcome(request.action, provider, status, detail),
            });
        }

        let mut process = leveler_execution::ProcessRequest::new(
            plan.program(),
            plan.args().to_vec(),
            plan.cwd().to_path_buf(),
        );
        process.timeout = timeout
            .map(|requested| requested.min(plan.timeout()))
            .unwrap_or_else(|| plan.timeout());
        process.max_output_bytes = max_output_bytes;
        let started = std::time::Instant::now();
        let output = runner.run(process, cancellation).await;
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
        let outcome = self.interpret(request.action, provider, &evidence);
        Ok(NativeRun {
            provider,
            capability: Some(capability),
            outcome,
        })
    }
}

fn blocked_outcome(
    action: ActionId,
    provider: ProviderId,
    status: ActionStatus,
    detail: String,
) -> NativeOutcome {
    NativeOutcome {
        action,
        provider,
        status,
        detail,
        execution_id: String::new(),
        exit_code: None,
        timed_out: false,
        sent_state: SentState::NotApplicable,
        delivery: DeliveryState::NotApplicable,
        evidence: vec!["executed=false".into()],
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    use crate::discover::DiscoveryReport;

    pub(super) fn discover() -> DiscoveryReport {
        DiscoveryReport {
            platform: std::env::consts::OS,
            supported: false,
            default_mail_handler: None,
            system_tools: Vec::new(),
            capabilities: Vec::new(),
            notes: vec![format!(
                "native actions are not implemented on {}; the build is unaffected",
                std::env::consts::OS
            )],
        }
    }
}
