//! A live probe of the host, not a fixture.
//!
//! The assertions pin the contract that must hold on any macOS host; the
//! printed report is the evidence a person reads. They deliberately do not
//! assert that Mail is installed or that consent is granted: those are
//! environment facts, and a test that assumed them would pass by luck rather
//! than prove classification.

#![cfg(target_os = "macos")]

use leveler_execution::CommandRunner;
use leveler_native::{CapabilityState, NativeActions, PermissionState};

#[tokio::test]
async fn macos_discovery_reports_live_mail_facts_without_overclaiming() {
    let report = NativeActions::new().discover(&CommandRunner::new()).await;
    eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());

    assert_eq!(report.platform, "macos");
    assert!(report.supported);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("default mailto handler is not probed")),
        "an unprobed field must say so"
    );
    assert_eq!(report.capabilities.len(), 1);
    let mail = &report.capabilities[0];
    assert_eq!(mail.id, "mail");
    assert_eq!(mail.provider, "macos.mail");
    assert_eq!(mail.actions.len(), 4);
    assert!(mail.requires_interactive_session);
    assert!(mail.requires_system_authorization);

    match mail.state {
        CapabilityState::Available => {
            assert_eq!(mail.permission, PermissionState::Granted);
            assert_eq!(mail.configured, Some(true));
            assert!(mail.reason.is_none());
        }
        CapabilityState::RequiresConfiguration => {
            assert_eq!(mail.configured, Some(false));
            assert!(mail.reason.is_some());
        }
        CapabilityState::RequiresPermission => {
            assert_ne!(mail.permission, PermissionState::Granted);
            assert!(mail.reason.is_some());
        }
        CapabilityState::Unavailable => {
            assert!(!mail.installed);
            assert!(mail.reason.is_some());
        }
        CapabilityState::Unknown => {
            assert!(mail.reason.is_some());
        }
        CapabilityState::Installed | CapabilityState::Unsupported => {
            panic!("macOS discovery must not report {:?}", mail.state);
        }
    }

    // Every report names where each fact came from, and no action ever claims
    // final delivery.
    assert!(!mail.evidence.is_empty());
    assert!(
        mail.evidence
            .iter()
            .any(|evidence| evidence.fact == "automation_permission")
    );
    assert!(
        mail.actions
            .iter()
            .all(|action| !action.delivery_confirmable)
    );
    assert!(
        report
            .system_tools
            .iter()
            .any(|tool| tool.id == "osascript" && tool.installed)
    );
}
