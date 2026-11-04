//! Discovery report types.
//!
//! Every field is either a fact observed by a platform probe or an explicit
//! unknown. Discovery never launches an application and never asks for a
//! permission prompt; it reports what it can prove without changing the host.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    /// The application or tool exists, but usability has not been established.
    Installed,
    /// Every precondition CodeLeveler can check now is satisfied.
    Available,
    /// The OS requires the user to grant automation permission, or the grant
    /// was refused.
    RequiresPermission,
    /// An account or profile the action needs is missing.
    RequiresConfiguration,
    /// The application or tool is not present.
    Unavailable,
    /// This build or platform has no implementation for the capability.
    Unsupported,
    /// A precondition could not be observed without a side effect discovery
    /// deliberately avoids. The reason names what is missing.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Granted,
    NotDetermined,
    Denied,
    Unknown,
    NotRequired,
}

impl PermissionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::NotDetermined => "not_determined",
            Self::Denied => "denied",
            Self::Unknown => "unknown",
            Self::NotRequired => "not_required",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnattendedSupport {
    Yes,
    /// Runs without interaction once the listed preconditions hold; the target
    /// application still needs a logged-in GUI session.
    Conditional,
    No,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub fact: String,
    pub value: String,
    pub source: String,
}

impl Evidence {
    pub(crate) fn new(
        fact: impl Into<String>,
        value: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            fact: fact.into(),
            value: value.into(),
            source: source.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionReport {
    pub id: &'static str,
    pub supported: bool,
    /// The action uses a structured interface rather than pixels and keystrokes.
    pub deterministic: bool,
    pub external_side_effects: bool,
    /// A person must complete the action in the UI.
    pub gui_only: bool,
    pub unattended: UnattendedSupport,
    pub sent_state_confirmable: bool,
    /// No mail provider in this phase can prove final delivery.
    pub delivery_confirmable: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityReport {
    pub id: &'static str,
    pub provider: &'static str,
    pub provider_name: &'static str,
    pub state: CapabilityState,
    pub installed: bool,
    pub permission: PermissionState,
    pub requires_system_authorization: bool,
    pub requires_interactive_session: bool,
    /// `Some(false)` is a positive observation that no usable account exists;
    /// `None` means discovery could not observe it without a side effect.
    pub configured: Option<bool>,
    pub reason: Option<String>,
    pub evidence: Vec<Evidence>,
    pub actions: Vec<ActionReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemToolReport {
    pub id: String,
    pub installed: bool,
    pub path: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryReport {
    pub platform: &'static str,
    pub supported: bool,
    /// The system handler the OS would use for `mailto:`. Reported as a fact,
    /// never as a send capability: opening a compose window is not sending.
    pub default_mail_handler: Option<String>,
    pub system_tools: Vec<SystemToolReport>,
    pub capabilities: Vec<CapabilityReport>,
    pub notes: Vec<String>,
}
