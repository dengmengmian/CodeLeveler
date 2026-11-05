//! Pure classification of observed facts into capability states.
//!
//! The platform modules gather facts; this module decides what they mean. It
//! is deliberately pure so the same state machine is testable on every host and
//! cannot quietly depend on the OS name.

use crate::discover::{
    ActionReport, CapabilityReport, CapabilityState, Evidence, PermissionState, UnattendedSupport,
};
use crate::model::ActionId;

/// Facts observed for Apple Mail. `accounts` stays `None` unless discovery
/// could actually query a running Mail without launching it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct MailProbe {
    pub installed: bool,
    pub install_path: String,
    pub permission: PermissionState,
    pub permission_detail: String,
    pub running: Option<bool>,
    pub accounts: Option<u32>,
    pub probe_error: Option<String>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_mail(probe: &MailProbe) -> CapabilityReport {
    let mut evidence = vec![
        Evidence::new(
            "application",
            if probe.installed {
                "present"
            } else {
                "missing"
            },
            format!("{} exists", probe.install_path),
        ),
        Evidence::new(
            "automation_permission",
            probe.permission.as_str(),
            "AEDeterminePermissionToAutomateTarget(ask=false)",
        ),
    ];
    if let Some(running) = probe.running {
        evidence.push(Evidence::new(
            "mail_running",
            if running { "true" } else { "false" },
            "pgrep -x Mail",
        ));
    }
    if let Some(accounts) = probe.accounts {
        evidence.push(Evidence::new(
            "enabled_accounts",
            accounts.to_string(),
            "Mail AppleScript account probe",
        ));
    }
    if let Some(error) = probe.probe_error.as_deref() {
        evidence.push(Evidence::new("probe_error", error, "live probe"));
    }

    let (state, reason, configured) = if !probe.installed {
        (
            CapabilityState::Unavailable,
            Some(format!(
                "Apple Mail is not installed at {}; no mail action can run",
                probe.install_path
            )),
            None,
        )
    } else if probe.permission == PermissionState::Denied {
        (
            CapabilityState::RequiresPermission,
            Some(
                "Automation permission for Mail was denied; grant it in System Settings > \
                 Privacy & Security > Automation and re-check"
                    .to_string(),
            ),
            None,
        )
    } else if probe.permission == PermissionState::NotDetermined {
        (
            CapabilityState::RequiresPermission,
            Some(
                "Apple Events automation consent has not been determined; the first action \
                 asks the user, and discovery does not prompt"
                    .to_string(),
            ),
            None,
        )
    } else if probe.permission == PermissionState::Unknown {
        (
            CapabilityState::Unknown,
            Some(format!(
                "Automation permission could not be determined: {}",
                probe.permission_detail
            )),
            None,
        )
    } else {
        match probe.accounts {
            Some(0) => (
                CapabilityState::RequiresConfiguration,
                Some("Mail has no enabled account with an email address".to_string()),
                Some(false),
            ),
            Some(_) => (CapabilityState::Available, None, Some(true)),
            None if probe.running == Some(false) => (
                CapabilityState::Unknown,
                Some(
                    "Mail is not running; discovery does not launch applications, so account \
                     configuration was not observed"
                        .to_string(),
                ),
                None,
            ),
            None => (
                CapabilityState::Unknown,
                Some(format!(
                    "Account configuration could not be verified: {}",
                    probe
                        .probe_error
                        .as_deref()
                        .unwrap_or("the account probe returned no result")
                )),
                None,
            ),
        }
    };
    let supported = probe.installed;
    let mut actions = actions(supported, true, reason.as_deref());
    if let Some(ui) = actions
        .iter_mut()
        .find(|action| action.id == ActionId::MailSendOpenUi.as_str())
    {
        ui.reason = Some("opens a compose window; a person must review and send".into());
    }

    CapabilityReport {
        id: "mail",
        provider: "macos.mail",
        provider_name: "Apple Mail",
        state,
        installed: probe.installed,
        configured,
        requires_system_authorization: true,
        requires_interactive_session: true,
        permission: probe.permission,
        reason,
        actions,
        evidence,
    }
}

/// Facts observed from the Windows registry and file system. No COM object is
/// created during discovery: creating one can start Outlook and can prompt.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct WindowsMailProbe {
    pub classic_progid: bool,
    pub classic_profile: bool,
    pub new_outlook: bool,
    pub default_handler: Option<String>,
    pub powershell: Option<String>,
    pub probe_errors: Vec<String>,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_mail(probe: &WindowsMailProbe) -> Vec<CapabilityReport> {
    let mut reports = Vec::new();
    let mut evidence = vec![
        Evidence::new(
            "classic_outlook_progid",
            probe.classic_progid.to_string(),
            "HKCR\\Outlook.Application",
        ),
        Evidence::new(
            "classic_outlook_profile",
            probe.classic_profile.to_string(),
            "HKCU\\Software\\Microsoft\\Office\\...\\Outlook\\Profiles",
        ),
        Evidence::new(
            "new_outlook_package",
            probe.new_outlook.to_string(),
            "HKCU AppModel package repository (Microsoft.OutlookForWindows)",
        ),
        Evidence::new(
            "default_mail_handler",
            probe
                .default_handler
                .clone()
                .unwrap_or_else(|| "none".into()),
            "HKCU mailto UserChoice ProgId",
        ),
        Evidence::new(
            "powershell",
            probe.powershell.clone().unwrap_or_else(|| "missing".into()),
            "%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
        ),
    ];
    for error in &probe.probe_errors {
        evidence.push(Evidence::new("probe_error", error.clone(), "live probe"));
    }

    if probe.classic_progid {
        let supported = probe.powershell.is_some();
        let reason = if supported {
            Some(
                "Classic Outlook is registered, but a registry key is not proof that COM can \
                 start it or that an account can send; discovery does not instantiate Outlook"
                    .to_string(),
            )
        } else {
            Some(
                "Classic Outlook is registered, but Windows PowerShell is not available, so \
                 there is no supported invocation path"
                    .to_string(),
            )
        };
        let mut actions = actions(true, supported, reason.as_deref());
        if supported
            && let Some(send) = actions
                .iter_mut()
                .find(|action| action.id == ActionId::MailSendSubmit.as_str())
        {
            send.reason = Some(
                "Outlook may show its own programmatic-send guard; the result reports what \
                 actually happened"
                    .into(),
            );
        }
        reports.push(CapabilityReport {
            id: "mail",
            provider: "windows.outlook.classic",
            provider_name: "Classic Outlook",
            state: if supported {
                CapabilityState::Installed
            } else {
                CapabilityState::Unsupported
            },
            installed: true,
            configured: None,
            requires_system_authorization: false,
            requires_interactive_session: true,
            permission: PermissionState::Unknown,
            reason,
            actions,
            evidence: evidence.clone(),
        });
    }

    if probe.new_outlook {
        let reason = Some(
            "New Outlook is installed, but this implementation has no supported automation \
             interface for it"
                .to_string(),
        );
        reports.push(CapabilityReport {
            id: "mail",
            provider: "windows.outlook.new",
            provider_name: "New Outlook",
            state: CapabilityState::Unsupported,
            installed: true,
            configured: None,
            requires_system_authorization: false,
            requires_interactive_session: true,
            permission: PermissionState::Unknown,
            reason: reason.clone(),
            actions: actions(true, false, reason.as_deref()),
            evidence: evidence.clone(),
        });
    }

    if reports.is_empty() {
        let reason = Some(
            probe
                .default_handler
                .as_deref()
                .map(|handler| {
                    format!(
                        "no supported mail client was found; the default mail handler is \
                         {handler}, which is not a supported automation provider"
                    )
                })
                .unwrap_or_else(|| "no supported mail client was found".to_string()),
        );
        reports.push(CapabilityReport {
            id: "mail",
            provider: "windows.mail",
            provider_name: "Mail",
            state: CapabilityState::Unavailable,
            installed: false,
            configured: None,
            requires_system_authorization: false,
            requires_interactive_session: true,
            permission: PermissionState::Unknown,
            reason: reason.clone(),
            actions: actions(false, false, reason.as_deref()),
            evidence,
        });
    }
    reports
}

fn actions(installed: bool, provider_supported: bool, reason: Option<&str>) -> Vec<ActionReport> {
    ActionId::ALL
        .into_iter()
        .map(|action| ActionReport {
            id: action.as_str(),
            supported: installed && provider_supported,
            deterministic: true,
            external_side_effects: action.external_side_effects(),
            gui_only: action.gui_only(),
            unattended: if !(installed && provider_supported) {
                UnattendedSupport::No
            } else if action.unattended_after_preconditions() {
                UnattendedSupport::Conditional
            } else {
                UnattendedSupport::No
            },
            sent_state_confirmable: action.sent_state_confirmable(),
            delivery_confirmable: action.delivery_confirmable(),
            reason: reason.map(str::to_string),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> MailProbe {
        MailProbe {
            installed: true,
            install_path: "/System/Applications/Mail.app".into(),
            permission: PermissionState::Granted,
            permission_detail: "granted".into(),
            running: Some(true),
            accounts: Some(1),
            probe_error: None,
        }
    }

    #[test]
    fn missing_mail_is_unavailable_and_claims_no_action() {
        let report = macos_mail(&MailProbe {
            installed: false,
            ..probe()
        });
        assert_eq!(report.state, CapabilityState::Unavailable);
        assert!(!report.installed);
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("not installed"),
            "{:?}",
            report.reason
        );
        assert!(report.actions.iter().all(|action| !action.supported));
    }

    #[test]
    fn undetermined_automation_consent_is_requires_permission_not_available() {
        let report = macos_mail(&MailProbe {
            permission: PermissionState::NotDetermined,
            permission_detail: "would require user consent".into(),
            ..probe()
        });
        assert_eq!(report.state, CapabilityState::RequiresPermission);
        assert!(report.installed);
        assert_eq!(report.configured, None);
        assert!(
            report.reason.as_deref().unwrap_or("").contains("consent"),
            "{:?}",
            report.reason
        );
    }

    #[test]
    fn denied_automation_is_reported_as_denied() {
        let report = macos_mail(&MailProbe {
            permission: PermissionState::Denied,
            ..probe()
        });
        assert_eq!(report.state, CapabilityState::RequiresPermission);
        assert!(
            report.reason.as_deref().unwrap_or("").contains("denied"),
            "{:?}",
            report.reason
        );
    }

    #[test]
    fn granted_but_not_running_is_unknown_instead_of_guessing_availability() {
        let report = macos_mail(&MailProbe {
            running: Some(false),
            accounts: None,
            ..probe()
        });
        assert_eq!(report.state, CapabilityState::Unknown);
        assert_eq!(report.configured, None);
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("not launch"),
            "{:?}",
            report.reason
        );
    }

    #[test]
    fn granted_with_no_account_is_requires_configuration() {
        let report = macos_mail(&MailProbe {
            accounts: Some(0),
            ..probe()
        });
        assert_eq!(report.state, CapabilityState::RequiresConfiguration);
        assert_eq!(report.configured, Some(false));
    }

    #[test]
    fn granted_with_an_enabled_account_is_the_only_available_path() {
        let report = macos_mail(&probe());
        assert_eq!(report.state, CapabilityState::Available);
        assert_eq!(report.configured, Some(true));
        assert!(report.actions.iter().all(|action| action.supported));
        let send = report
            .actions
            .iter()
            .find(|action| action.id == "mail.send.submit")
            .expect("send action");
        assert!(send.deterministic);
        assert!(send.external_side_effects);
        assert!(send.sent_state_confirmable);
        assert!(!send.delivery_confirmable);
        assert_eq!(send.unattended, UnattendedSupport::Conditional);
        let ui = report
            .actions
            .iter()
            .find(|action| action.id == "mail.send.open_ui")
            .expect("ui action");
        assert!(ui.gui_only);
        assert_eq!(ui.unattended, UnattendedSupport::No);
    }

    fn windows() -> WindowsMailProbe {
        WindowsMailProbe {
            classic_progid: true,
            classic_profile: true,
            new_outlook: true,
            default_handler: Some("Outlook.URL.mailto.15".into()),
            powershell: Some(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe".into()),
            probe_errors: Vec::new(),
        }
    }

    #[test]
    fn classic_outlook_is_installed_but_never_claimed_available_from_registration() {
        let reports = windows_mail(&windows());
        let classic = reports
            .iter()
            .find(|report| report.provider == "windows.outlook.classic")
            .expect("classic report");
        assert_eq!(classic.state, CapabilityState::Installed);
        assert_eq!(classic.configured, None);
        assert!(
            classic
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("not instantiate"),
            "{:?}",
            classic.reason
        );
        assert!(classic.actions.iter().any(|action| action.supported));
    }

    #[test]
    fn new_outlook_is_a_separate_unsupported_provider() {
        let reports = windows_mail(&windows());
        let new = reports
            .iter()
            .find(|report| report.provider == "windows.outlook.new")
            .expect("new outlook report");
        assert_eq!(new.state, CapabilityState::Unsupported);
        assert!(new.installed);
        assert!(new.actions.iter().all(|action| !action.supported));
        assert!(
            new.reason
                .as_deref()
                .unwrap_or("")
                .contains("no supported automation"),
            "{:?}",
            new.reason
        );
    }

    #[test]
    fn default_handler_is_reported_as_a_fact_not_a_send_capability() {
        let reports = windows_mail(&windows());
        assert!(
            reports
                .iter()
                .all(|report| report.provider != "windows.default-handler")
        );
        assert!(
            reports
                .iter()
                .filter(|report| report.state == CapabilityState::Available)
                .count()
                == 0
        );
    }

    #[test]
    fn no_windows_mail_client_is_unavailable() {
        let reports = windows_mail(&WindowsMailProbe {
            classic_progid: false,
            classic_profile: false,
            new_outlook: false,
            default_handler: None,
            powershell: None,
            probe_errors: Vec::new(),
        });
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, CapabilityState::Unavailable);
        assert!(!reports[0].installed);
    }

    #[test]
    fn missing_powershell_makes_classic_actions_unsupported() {
        let reports = windows_mail(&WindowsMailProbe {
            powershell: None,
            ..windows()
        });
        let classic = reports
            .iter()
            .find(|report| report.provider == "windows.outlook.classic")
            .expect("classic report");
        assert!(classic.actions.iter().all(|action| !action.supported));
        assert!(
            classic
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("PowerShell"),
            "{:?}",
            classic.reason
        );
    }
}
