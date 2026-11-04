//! Structured input validation and result interpretation.
//!
//! User content never reaches a script: it is written to private payload files
//! and the script reads those files. These rules are the first gate.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::model::{
    ActionId, ActionStatus, DeliveryState, ExecutionEvidence, NativeError, NativeOutcome,
    ProviderId, SentState,
};

pub(crate) const MAX_SUBJECT_CHARS: usize = 512;
pub(crate) const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub(crate) enum Payload {
    Message {
        to: String,
        subject: String,
        body: String,
    },
    Check {
        to: String,
        subject: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageInput {
    to: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckInput {
    to: String,
    #[serde(default)]
    subject: String,
}

pub(crate) fn parse_input(
    action: ActionId,
    input: &serde_json::Value,
) -> Result<Payload, NativeError> {
    let decode = |error: serde_json::Error| NativeError::InvalidInput(error.to_string());
    match action {
        ActionId::MailSentCheck => {
            let input: CheckInput = serde_json::from_value(input.clone()).map_err(decode)?;
            validate_recipient(&input.to).map_err(NativeError::InvalidInput)?;
            validate_subject(&input.subject).map_err(NativeError::InvalidInput)?;
            Ok(Payload::Check {
                to: input.to,
                subject: input.subject,
            })
        }
        ActionId::MailDraftCreate | ActionId::MailSendOpenUi | ActionId::MailSendSubmit => {
            let input: MessageInput = serde_json::from_value(input.clone()).map_err(decode)?;
            validate_recipient(&input.to).map_err(NativeError::InvalidInput)?;
            validate_subject(&input.subject).map_err(NativeError::InvalidInput)?;
            validate_body(&input.body).map_err(NativeError::InvalidInput)?;
            Ok(Payload::Message {
                to: input.to,
                subject: input.subject,
                body: input.body,
            })
        }
    }
}

pub(crate) fn validate_recipient(to: &str) -> Result<(), String> {
    if to.is_empty() {
        return Err("recipient is empty".into());
    }
    if to.len() > 320 {
        return Err("recipient is longer than 320 bytes".into());
    }
    if to.chars().any(|character| character.is_control()) {
        return Err("recipient contains a control character".into());
    }
    if to.chars().any(char::is_whitespace) {
        return Err(
            "recipient contains whitespace; use exactly one plain address, not a display name"
                .into(),
        );
    }
    if to.matches('@').count() != 1 {
        return Err("recipient must contain exactly one @".into());
    }
    let (local, domain) = to.split_once('@').expect("one @ was checked");
    if local.is_empty() || local.len() > 64 {
        return Err("recipient local part must be 1-64 bytes".into());
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err("recipient local part has an invalid dot".into());
    }
    const LOCAL: &str = ".!#$%&'*+/=?^_`{|}~-";
    if !local
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || LOCAL.contains(character))
    {
        return Err("recipient local part contains an unsupported character".into());
    }
    if domain.is_empty() || domain.len() > 253 {
        return Err("recipient domain is empty or too long".into());
    }
    if !domain.contains('.') {
        return Err("recipient domain must contain a dot".into());
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err("recipient domain has an invalid dot".into());
    }
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("recipient domain label must be 1-63 bytes".into());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("recipient domain label starts or ends with a hyphen".into());
        }
        if !label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err("recipient domain contains an unsupported character".into());
        }
    }
    let tld = domain.rsplit('.').next().unwrap_or_default();
    if tld.len() < 2 || !tld.chars().any(|character| character.is_ascii_alphabetic()) {
        return Err("recipient domain has no alphabetic top-level label".into());
    }
    Ok(())
}

pub(crate) fn validate_subject(subject: &str) -> Result<(), String> {
    if subject.chars().count() > MAX_SUBJECT_CHARS {
        return Err(format!(
            "subject is longer than {MAX_SUBJECT_CHARS} characters"
        ));
    }
    if subject.chars().any(char::is_control) {
        return Err("subject contains a control character or a line break".into());
    }
    Ok(())
}

pub(crate) fn validate_body(body: &str) -> Result<(), String> {
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("body is longer than {MAX_BODY_BYTES} bytes"));
    }
    if body.contains('\0') {
        return Err("body contains a NUL byte".into());
    }
    Ok(())
}

/// Write a payload file only the current user can read. The directory itself
/// is created by `tempfile` with owner-only permissions.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

pub fn interpret(
    action: ActionId,
    provider: ProviderId,
    evidence: &ExecutionEvidence,
) -> NativeOutcome {
    let mut observed = vec![
        format!("exit_code={:?}", evidence.exit_code),
        format!("duration_ms={}", evidence.duration_ms),
    ];
    if evidence.timed_out {
        observed.push("timed_out=true".into());
    }
    let stdout = evidence.stdout.trim();
    let stderr = sanitize(&evidence.stderr);
    let marker = recognize(stdout);

    let (status, detail, sent_state, delivery, extra) = if evidence.timed_out {
        (
            ActionStatus::Unknown,
            "the action did not finish before its deadline; its effect is unknown and it was not retried".to_string(),
            SentState::Unknown,
            if action == ActionId::MailSendSubmit {
                DeliveryState::NotVerifiable
            } else {
                DeliveryState::NotApplicable
            },
            Vec::new(),
        )
    } else if evidence.exit_code != Some(0) {
        classify_failure(stdout, &stderr)
    } else if let Some(marker) = marker {
        classify_success(action, marker, &mut observed)
    } else {
        (
            ActionStatus::Unknown,
            "the helper exited 0 without a recognized result marker; no effect is claimed".into(),
            SentState::Unknown,
            DeliveryState::NotApplicable,
            Vec::new(),
        )
    };
    observed.extend(extra);

    NativeOutcome {
        action,
        provider,
        status,
        detail,
        execution_id: execution_id(),
        exit_code: evidence.exit_code,
        timed_out: evidence.timed_out,
        sent_state,
        delivery,
        evidence: observed,
    }
}

enum Marker {
    SendSubmitted,
    DraftCreated,
    ComposeOpened,
    SentCounts {
        found: u32,
        scanned: u32,
        total: u32,
    },
}

fn recognize(stdout: &str) -> Option<Marker> {
    let line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    let line = line.trim();
    if line == "SEND_SUBMITTED" {
        return Some(Marker::SendSubmitted);
    }
    if line == "DRAFT_CREATED" {
        return Some(Marker::DraftCreated);
    }
    if line == "COMPOSE_OPENED" {
        return Some(Marker::ComposeOpened);
    }
    let mut parts = line.split_whitespace();
    if parts.next()? != "SENT_FOUND" {
        return None;
    }
    let found = parts.next()?.parse().ok()?;
    if parts.next()? != "SCANNED" {
        return None;
    }
    let scanned = parts.next()?.parse().ok()?;
    if parts.next()? != "TOTAL" {
        return None;
    }
    let total = parts.next()?.parse().ok()?;
    Some(Marker::SentCounts {
        found,
        scanned,
        total,
    })
}

fn classify_success(
    action: ActionId,
    marker: Marker,
    observed: &mut Vec<String>,
) -> (ActionStatus, String, SentState, DeliveryState, Vec<String>) {
    match (action, marker) {
        (ActionId::MailSendSubmit, Marker::SendSubmitted) => (
            ActionStatus::Submitted,
            "the mail application accepted the message for delivery; delivery is not \
             confirmable through this interface"
                .into(),
            SentState::Submitted,
            DeliveryState::NotVerifiable,
            Vec::new(),
        ),
        (ActionId::MailDraftCreate, Marker::DraftCreated) => (
            ActionStatus::Submitted,
            "the mail application accepted the draft; its existence in Drafts was not \
             verified by this action"
                .into(),
            SentState::NotApplicable,
            DeliveryState::NotApplicable,
            Vec::new(),
        ),
        (ActionId::MailSendOpenUi, Marker::ComposeOpened) => (
            ActionStatus::Submitted,
            "the mail application was asked to open a compose window; a person must review \
             and send it"
                .into(),
            SentState::NotApplicable,
            DeliveryState::NotApplicable,
            Vec::new(),
        ),
        (
            ActionId::MailSentCheck,
            Marker::SentCounts {
                found,
                scanned,
                total,
            },
        ) => {
            observed.push(format!("sent_found={found}"));
            observed.push(format!("scanned={scanned}"));
            observed.push(format!("sent_total={total}"));
            let (state, detail) = if found > 0 {
                (
                    SentState::ConfirmedInSentMailbox,
                    format!(
                        "found {found} matching message(s) in Sent after scanning {scanned} of \
                         {total}; a Sent record is not a delivery receipt"
                    ),
                )
            } else {
                (
                    SentState::NotFoundInScan,
                    format!(
                        "no matching message among the {scanned} most recent Sent messages of \
                         {total}; absence is not proof that a message was not sent"
                    ),
                )
            };
            (
                ActionStatus::Completed,
                detail,
                state,
                DeliveryState::NotApplicable,
                Vec::new(),
            )
        }
        (_, marker) => (
            ActionStatus::Unknown,
            format!(
                "the helper reported a result that does not belong to {}; no effect is claimed",
                action.as_str()
            ),
            SentState::Unknown,
            DeliveryState::NotApplicable,
            vec![format!("marker={}", marker_name(&marker))],
        ),
    }
}

fn marker_name(marker: &Marker) -> &'static str {
    match marker {
        Marker::SendSubmitted => "send_submitted",
        Marker::DraftCreated => "draft_created",
        Marker::ComposeOpened => "compose_opened",
        Marker::SentCounts { .. } => "sent_counts",
    }
}

fn classify_failure(
    stdout: &str,
    stderr: &str,
) -> (ActionStatus, String, SentState, DeliveryState, Vec<String>) {
    let both = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    if both.contains("-1743")
        || both.contains("not authorized to send apple events")
        || both.contains("permission_denied")
        || both.contains("access is denied")
    {
        return (
            ActionStatus::RequiresPermission,
            "the OS or the target application refused automation permission; the action did \
             not run"
                .into(),
            SentState::Unknown,
            DeliveryState::NotApplicable,
            Vec::new(),
        );
    }
    if both.contains("-1744") || both.contains("would require user consent") {
        return (
            ActionStatus::RequiresPermission,
            "automation consent has not been granted yet; the action needs an interactive \
             consent before it can run"
                .into(),
            SentState::Unknown,
            DeliveryState::NotApplicable,
            Vec::new(),
        );
    }
    if both.contains("com_unavailable") {
        return (
            ActionStatus::Unsupported,
            "the classic Outlook COM automation interface is not available on this host".into(),
            SentState::Unknown,
            DeliveryState::NotApplicable,
            Vec::new(),
        );
    }
    if both.contains("account_missing") || both.contains("no enabled account") {
        return (
            ActionStatus::RequiresConfiguration,
            "the mail application has no enabled account that can send".into(),
            SentState::Unknown,
            DeliveryState::NotApplicable,
            Vec::new(),
        );
    }
    if both.contains("-600") || both.contains("application isn't running") {
        return (
            ActionStatus::Failed,
            "the target mail application is not running and could not be launched".into(),
            SentState::Unknown,
            DeliveryState::NotApplicable,
            Vec::new(),
        );
    }
    let summary = if stderr.is_empty() {
        "the helper failed without a diagnostic".to_string()
    } else {
        format!("the helper failed: {stderr}")
    };
    (
        ActionStatus::Failed,
        summary,
        SentState::Unknown,
        DeliveryState::NotApplicable,
        Vec::new(),
    )
}

/// Keep one bounded line: a diagnostic must not carry user content or a
/// multi-line payload into a log or a model context.
fn sanitize(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut line: String = line
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if line.chars().count() > 300 {
        line = line.chars().take(300).collect();
        line.push('…');
    }
    line
}

fn execution_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("native-{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(to: &str, subject: &str, body: &str) -> serde_json::Value {
        json!({"to": to, "subject": subject, "body": body})
    }

    #[test]
    fn one_plain_address_is_accepted() {
        assert!(validate_recipient("alice@example.com").is_ok());
        assert!(validate_recipient("first.last+tag@mail.example.co.uk").is_ok());
    }

    #[test]
    fn multiple_recipients_and_display_names_are_rejected() {
        for invalid in [
            "a@example.com, b@example.com",
            "a@example.com;b@example.com",
            "Alice <alice@example.com>",
            "alice@example.com ",
        ] {
            assert!(validate_recipient(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn header_injection_in_recipient_or_subject_is_rejected() {
        assert!(validate_recipient("a@example.com\nBcc: evil@example.com").is_err());
        assert!(validate_subject("hello\nBcc: evil@example.com").is_err());
        assert!(validate_subject("hello\rworld").is_err());
    }

    #[test]
    fn subject_and_body_limits_are_enforced() {
        assert!(validate_subject(&"a".repeat(MAX_SUBJECT_CHARS)).is_ok());
        assert!(validate_subject(&"a".repeat(MAX_SUBJECT_CHARS + 1)).is_err());
        assert!(validate_body(&"a".repeat(MAX_BODY_BYTES)).is_ok());
        assert!(validate_body(&"a".repeat(MAX_BODY_BYTES + 1)).is_err());
        assert!(validate_body("nul\0byte").is_err());
    }

    #[test]
    fn body_keeps_quotes_newlines_and_unicode_exactly() {
        let body = "第一行\n\"quoted\" $(rm -rf /) `backtick` — done\r\nend";
        assert!(validate_body(body).is_ok());
        let parsed = parse_input(
            ActionId::MailSendSubmit,
            &message("a@example.com", "hi", body),
        )
        .expect("valid input");
        let Payload::Message { body: kept, .. } = parsed else {
            panic!("message payload");
        };
        assert_eq!(kept, body);
    }

    #[test]
    fn unknown_fields_are_rejected_instead_of_ignored() {
        let error = parse_input(
            ActionId::MailSendSubmit,
            &json!({"to": "a@example.com", "subject": "s", "body": "b", "bcc": "x@y.z"}),
        )
        .expect_err("unknown field");
        assert!(error.to_string().contains("bcc"), "{error}");
    }

    #[test]
    fn a_missing_recipient_is_an_input_error() {
        assert!(parse_input(ActionId::MailSendSubmit, &json!({"subject": "s"})).is_err());
        assert!(parse_input(ActionId::MailSentCheck, &json!({"subject": "s"})).is_err());
    }

    #[test]
    fn draft_and_ui_accept_the_same_structured_message() {
        for action in [ActionId::MailDraftCreate, ActionId::MailSendOpenUi] {
            let parsed =
                parse_input(action, &message("a@example.com", "subject", "body")).expect("valid");
            assert!(matches!(parsed, Payload::Message { .. }));
        }
        let checked = parse_input(
            ActionId::MailSentCheck,
            &json!({"to": "a@example.com", "subject": "subject"}),
        )
        .expect("valid");
        assert!(matches!(checked, Payload::Check { .. }));
    }

    #[test]
    fn a_successful_submit_is_submitted_never_delivered() {
        let outcome = interpret(
            ActionId::MailSendSubmit,
            ProviderId::MacosMail,
            &ExecutionEvidence {
                exit_code: Some(0),
                stdout: "SEND_SUBMITTED\n".into(),
                ..Default::default()
            },
        );
        assert_eq!(outcome.status, ActionStatus::Submitted);
        assert_eq!(outcome.sent_state, SentState::Submitted);
        assert_eq!(outcome.delivery, DeliveryState::NotVerifiable);
        assert!(
            outcome.detail.contains("not confirmable"),
            "{}",
            outcome.detail
        );
    }

    #[test]
    fn a_timeout_is_unknown_and_is_not_retried() {
        let outcome = interpret(
            ActionId::MailSendSubmit,
            ProviderId::MacosMail,
            &ExecutionEvidence {
                exit_code: None,
                timed_out: true,
                ..Default::default()
            },
        );
        assert_eq!(outcome.status, ActionStatus::Unknown);
        assert_eq!(outcome.sent_state, SentState::Unknown);
        assert!(outcome.detail.contains("not retried"), "{}", outcome.detail);
    }

    #[test]
    fn a_zero_exit_without_the_result_marker_claims_nothing() {
        let outcome = interpret(
            ActionId::MailSendSubmit,
            ProviderId::MacosMail,
            &ExecutionEvidence {
                exit_code: Some(0),
                stdout: "some unrelated output".into(),
                ..Default::default()
            },
        );
        assert_eq!(outcome.status, ActionStatus::Unknown);
        assert_eq!(outcome.sent_state, SentState::Unknown);
    }

    #[test]
    fn apple_events_denial_is_permission_not_failure() {
        let outcome = interpret(
            ActionId::MailSendSubmit,
            ProviderId::MacosMail,
            &ExecutionEvidence {
                exit_code: Some(1),
                stderr: "execution error: Not authorized to send Apple events to Mail. (-1743)"
                    .into(),
                ..Default::default()
            },
        );
        assert_eq!(outcome.status, ActionStatus::RequiresPermission);
    }

    #[test]
    fn missing_outlook_com_is_unsupported_not_success() {
        let outcome = interpret(
            ActionId::MailSendSubmit,
            ProviderId::WindowsOutlookClassic,
            &ExecutionEvidence {
                exit_code: Some(20),
                stdout: "ERROR COM_UNAVAILABLE".into(),
                ..Default::default()
            },
        );
        assert_eq!(outcome.status, ActionStatus::Unsupported);
    }

    #[test]
    fn sent_check_reports_what_it_scanned_without_claiming_delivery() {
        let outcome = interpret(
            ActionId::MailSentCheck,
            ProviderId::MacosMail,
            &ExecutionEvidence {
                exit_code: Some(0),
                stdout: "SENT_FOUND 1 SCANNED 200 TOTAL 900\n".into(),
                ..Default::default()
            },
        );
        assert_eq!(outcome.status, ActionStatus::Completed);
        assert_eq!(outcome.sent_state, SentState::ConfirmedInSentMailbox);
        assert_eq!(outcome.delivery, DeliveryState::NotApplicable);
        assert!(
            outcome
                .evidence
                .iter()
                .any(|line| line.contains("scanned=200"))
        );
    }

    #[test]
    fn sent_check_absence_is_not_proof() {
        let outcome = interpret(
            ActionId::MailSentCheck,
            ProviderId::MacosMail,
            &ExecutionEvidence {
                exit_code: Some(0),
                stdout: "SENT_FOUND 0 SCANNED 12 TOTAL 12\n".into(),
                ..Default::default()
            },
        );
        assert_eq!(outcome.sent_state, SentState::NotFoundInScan);
        assert!(outcome.detail.contains("not proof"), "{}", outcome.detail);
    }

    #[test]
    fn a_draft_is_submitted_without_a_sent_or_delivery_claim() {
        let outcome = interpret(
            ActionId::MailDraftCreate,
            ProviderId::MacosMail,
            &ExecutionEvidence {
                exit_code: Some(0),
                stdout: "DRAFT_CREATED\n".into(),
                ..Default::default()
            },
        );
        assert_eq!(outcome.status, ActionStatus::Submitted);
        assert_eq!(outcome.sent_state, SentState::NotApplicable);
        assert_eq!(outcome.delivery, DeliveryState::NotApplicable);
        assert!(
            outcome.detail.contains("not verified"),
            "{}",
            outcome.detail
        );
    }

    #[test]
    fn every_outcome_has_an_audit_identity() {
        let outcome = interpret(
            ActionId::MailSentCheck,
            ProviderId::MacosMail,
            &ExecutionEvidence::default(),
        );
        assert!(outcome.execution_id.starts_with("native-"));
        assert!(outcome.execution_id.len() > 12);
    }
}
