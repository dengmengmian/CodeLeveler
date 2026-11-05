//! The stable vocabulary shared by discovery, planning and result reporting.
//!
//! These types carry facts, never a plan or an approval decision: the caller
//! decides whether an action is permitted and the host execution authority
//! performs the real side effect.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One deterministic local action. The dotted id is the audit identity a tool
/// call, a CLI invocation and a future schedule all name identically.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
// The enum is the shared native-action vocabulary, not a mail-only enum: the
// `Mail` prefix is the action's domain and is part of its wire identity. Future
// domains add their own prefix instead of making `Create` mean one thing today
// and another tomorrow.
#[allow(clippy::enum_variant_names)]
pub enum ActionId {
    #[serde(rename = "mail.draft.create")]
    MailDraftCreate,
    #[serde(rename = "mail.send.open_ui")]
    MailSendOpenUi,
    #[serde(rename = "mail.send.submit")]
    MailSendSubmit,
    #[serde(rename = "mail.sent.check")]
    MailSentCheck,
}

impl ActionId {
    pub const ALL: [Self; 4] = [
        Self::MailDraftCreate,
        Self::MailSendOpenUi,
        Self::MailSendSubmit,
        Self::MailSentCheck,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MailDraftCreate => "mail.draft.create",
            Self::MailSendOpenUi => "mail.send.open_ui",
            Self::MailSendSubmit => "mail.send.submit",
            Self::MailSentCheck => "mail.sent.check",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
            .ok_or_else(|| {
                format!(
                    "unknown native action {value:?}; expected one of {}",
                    Self::ALL.map(Self::as_str).join(", ")
                )
            })
    }

    /// Whether the action asks an external application to change something
    /// outside CodeLeveler. Reading the Sent mailbox is not one.
    pub const fn external_side_effects(self) -> bool {
        !matches!(self, Self::MailSentCheck)
    }

    /// Whether a person must click something before the action can complete.
    pub const fn gui_only(self) -> bool {
        matches!(self, Self::MailSendOpenUi)
    }

    /// Whether a fresh action can run with no interaction once the discovered
    /// preconditions hold. `open_ui` deliberately requires a person.
    pub const fn unattended_after_preconditions(self) -> bool {
        !matches!(self, Self::MailSendOpenUi)
    }

    /// Whether the action drives an application that performs network I/O on
    /// its own behalf. The helper process cannot confine that I/O, so a turn
    /// that denies network must refuse the action rather than silently let the
    /// target application use it.
    pub const fn drives_networked_application(self) -> bool {
        true
    }

    /// Whether a Sent-mailbox record can be checked after the action. Only the
    /// submit action produces one.
    pub const fn sent_state_confirmable(self) -> bool {
        matches!(self, Self::MailSendSubmit)
    }

    /// No mail automation interface available to CodeLeveler reports final
    /// delivery. A submitted message and a Sent-mailbox record are not delivery
    /// receipts.
    pub const fn delivery_confirmable(self) -> bool {
        false
    }
}

/// Structured input for one action.
#[derive(Debug, Clone)]
pub struct NativeActionRequest {
    pub action: ActionId,
    pub input: serde_json::Value,
}

/// Provider identity, so an outcome says which application actually ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ProviderId {
    #[serde(rename = "macos.mail")]
    MacosMail,
    #[serde(rename = "windows.outlook.classic")]
    WindowsOutlookClassic,
}

impl ProviderId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MacosMail => "macos.mail",
            Self::WindowsOutlookClassic => "windows.outlook.classic",
        }
    }
}

/// One fully prepared execution. The payload directory stays alive for as long
/// as this value; dropping it removes the private files.
#[derive(Debug)]
pub struct ActionPlan {
    pub(crate) action: ActionId,
    pub(crate) provider: ProviderId,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) timeout: Duration,
    /// Kept alive for the lifetime of the plan: dropping it removes the
    /// private payload files the action reads.
    pub(crate) _payload: tempfile::TempDir,
}

impl ActionPlan {
    pub fn action(&self) -> ActionId {
        self.action
    }
    pub fn provider(&self) -> ProviderId {
        self.provider
    }
    pub fn program(&self) -> &str {
        &self.program
    }
    pub fn args(&self) -> &[String] {
        &self.args
    }
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// The process facts the interpreter is allowed to use. Deliberately not a
/// success flag: the caller's process exit code alone never proves the action
/// completed.
#[derive(Debug, Clone, Default)]
pub struct ExecutionEvidence {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// The action produced a result CodeLeveler verified (a Sent-mailbox query
    /// returning its counts).
    Completed,
    /// The target application accepted the request; the final effect is not
    /// verified.
    Submitted,
    /// The process exited but the result cannot be established. Never treated
    /// as success and never retried automatically.
    Unknown,
    /// The OS or the application refused authorization.
    RequiresPermission,
    /// An observed precondition (account, application) is missing.
    RequiresConfiguration,
    Failed,
    Unsupported,
}

impl ActionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Submitted => "submitted",
            Self::Unknown => "unknown",
            Self::RequiresPermission => "requires_permission",
            Self::RequiresConfiguration => "requires_configuration",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SentState {
    /// The action does not create a Sent-mailbox record.
    NotApplicable,
    /// The application accepted the send; no Sent-mailbox check was made.
    Submitted,
    /// A message with the exact subject and recipient was found in Sent.
    ConfirmedInSentMailbox,
    /// Nothing matched in the bounded scan. This is not proof of non-delivery.
    NotFoundInScan,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    NotApplicable,
    /// No available interface reports final delivery.
    NotVerifiable,
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeOutcome {
    pub action: ActionId,
    pub provider: ProviderId,
    pub status: ActionStatus,
    pub detail: String,
    pub execution_id: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub sent_state: SentState,
    pub delivery: DeliveryState,
    pub evidence: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("native action {0} is not supported here: {1}")]
    Unsupported(&'static str, String),
    #[error("could not prepare {action}: {detail}")]
    Prepare {
        action: &'static str,
        detail: String,
    },
}
