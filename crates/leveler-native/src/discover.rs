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

impl CapabilityState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Available => "available",
            Self::RequiresPermission => "requires_permission",
            Self::RequiresConfiguration => "requires_configuration",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
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

/// How much interaction the caller allows before execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// A person may answer an OS consent prompt (interactive tool/CLI path).
    MayPrompt,
    /// Nobody is present. A permission that is not already granted blocks the
    /// action instead of prompting or hanging.
    Unattended,
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

impl CapabilityReport {
    /// Why this capability cannot start the action now, and the status the
    /// caller must report if it does not start. `None` means admission may
    /// proceed; runtime failure is still interpreted by the action itself.
    pub fn admission_block(
        &self,
        action: crate::ActionId,
        mode: PermissionMode,
    ) -> Option<(crate::ActionStatus, String)> {
        use crate::{ActionStatus, PermissionState};

        let action_report = self
            .actions
            .iter()
            .find(|candidate| candidate.id == action.as_str());
        if !action_report.is_some_and(|report| report.supported) {
            let reason = action_report
                .and_then(|report| report.reason.as_deref())
                .or(self.reason.as_deref())
                .unwrap_or("no supported capability was reported");
            return Some((ActionStatus::Unsupported, reason.to_string()));
        }

        let block = |status, fallback: &str| {
            Some((
                status,
                self.reason.clone().unwrap_or_else(|| fallback.to_string()),
            ))
        };
        match self.state {
            CapabilityState::Unavailable | CapabilityState::Unsupported => block(
                ActionStatus::Unsupported,
                "the capability is not available on this host",
            ),
            CapabilityState::RequiresPermission => {
                if self.permission == PermissionState::NotDetermined
                    && mode == PermissionMode::MayPrompt
                {
                    None
                } else if self.permission == PermissionState::NotDetermined {
                    Some((
                        ActionStatus::RequiresPermission,
                        self.reason.clone().unwrap_or_else(|| {
                            "the OS permission has not been granted and an unattended run \
                             never prompts"
                                .to_string()
                        }),
                    ))
                } else {
                    block(
                        ActionStatus::RequiresPermission,
                        "automation permission is required",
                    )
                }
            }
            CapabilityState::RequiresConfiguration => block(
                ActionStatus::RequiresConfiguration,
                "an account or application setting must be configured first",
            ),
            CapabilityState::Unknown if self.permission == PermissionState::Unknown => block(
                ActionStatus::RequiresPermission,
                "automation permission could not be determined",
            ),
            CapabilityState::Installed | CapabilityState::Available | CapabilityState::Unknown => {
                None
            }
        }
    }
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
