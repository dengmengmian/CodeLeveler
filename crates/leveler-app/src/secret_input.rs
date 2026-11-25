//! Turn captured credentials into a message the runtime can safely admit.
//!
//! This is the single admission transform for user text. It does two things
//! and nothing else:
//!
//! 1. Replaces each client placeholder with an opaque `[secret:sec_…]` token
//!    after taking custody of the value in the host vault.
//! 2. Refuses text that still looks like a high-confidence credential without
//!    the person's explicit plaintext acknowledgement.
//!
//! Detection never rewrites text by itself: an ambiguous value passes through
//! unchanged (the client is expected to have asked), and ordinary code is not
//! touched. Refusal is the only automatic outcome for an unacknowledged
//! high-confidence credential, because the alternative — silently sending it
//! to a provider — is exactly the accident this module exists to prevent.

use leveler_client_protocol::ClientError;
use leveler_client_protocol::SessionId;
use leveler_client_protocol::secret::SecretCapture;
use leveler_core::{SecretLevel, detect_secrets};
use leveler_execution::secret_vault::{SecretRef, SecretVault};

/// Most captures one message may carry. A message with more is a data dump,
/// not a request.
const MAX_CAPTURES_PER_MESSAGE: usize = 16;

/// Longest placeholder a client may choose.
const MAX_PLACEHOLDER_BYTES: usize = 256;

/// Prepare `content` for admission.
pub(crate) fn prepare_secret_input(
    vault: &SecretVault,
    session_id: &SessionId,
    content: &str,
    captures: &[SecretCapture],
    plaintext_ack: bool,
) -> Result<String, ClientError> {
    // The person made an explicit decision. It is recorded as their choice,
    // not as a detection result: do not rewrite what they asked to send.
    if plaintext_ack {
        return Ok(content.to_string());
    }
    if captures.len() > MAX_CAPTURES_PER_MESSAGE {
        return Err(ClientError::Runtime(format!(
            "一条消息最多接管 {MAX_CAPTURES_PER_MESSAGE} 个凭据"
        )));
    }
    if captures.is_empty() {
        return match high_confidence_summary(content) {
            None => Ok(content.to_string()),
            Some(summary) => Err(ClientError::Runtime(format!(
                "消息中检测到疑似凭据（{summary}），但尚未安全接管。请确认后重试：                 在 TUI 中选择“安全接管”，或先用 /secret 粘贴凭据。"
            ))),
        };
    }

    // Validate every placeholder before taking custody of any value, so a
    // malformed request cannot leave the vault with unused captures.
    for (index, capture) in captures.iter().enumerate() {
        if capture.placeholder.is_empty()
            || capture.placeholder.len() > MAX_PLACEHOLDER_BYTES
            || capture.value.is_empty()
        {
            return Err(ClientError::Runtime("凭据接管的占位符或值为空".to_string()));
        }
        if captures[..index]
            .iter()
            .any(|earlier| earlier.placeholder == capture.placeholder)
        {
            return Err(ClientError::Runtime("凭据接管的占位符重复".to_string()));
        }
        let occurrences = content.matches(capture.placeholder.as_str()).count();
        if occurrences != 1 {
            return Err(ClientError::Runtime(format!(
                "凭据占位符必须在消息中恰好出现一次（实际 {occurrences} 次）"
            )));
        }
    }

    let session = session_id.as_str();
    let mut created: Vec<SecretRef> = Vec::new();
    let mut by_value: Vec<(&str, SecretRef)> = Vec::new();
    let mut out = content.to_string();
    for capture in captures {
        let value = capture.value.expose();
        let reference =
            if let Some((_, reference)) = by_value.iter().find(|(known, _)| *known == value) {
                // The same value appearing twice is one credential: one capture,
                // one reference, so a later revoke covers every occurrence.
                reference.clone()
            } else {
                match vault.capture(session, value.to_string(), capture.key_hint.clone()) {
                    Ok(reference) => {
                        created.push(reference.clone());
                        by_value.push((value, reference.clone()));
                        reference
                    }
                    Err(error) => {
                        for created in &created {
                            vault.revoke(session, created);
                        }
                        return Err(ClientError::Runtime(vault_message(error)));
                    }
                }
            };
        out = out.replace(
            capture.placeholder.as_str(),
            &format!("[secret:{}]", reference.as_str()),
        );
    }

    // A capture is not a licence to send the rest: if the message still
    // carries an uncaptured high-confidence credential, nothing is admitted.
    if let Some(summary) = high_confidence_summary(&out) {
        for created in &created {
            vault.revoke(session, created);
        }
        return Err(ClientError::Runtime(format!(
            "消息中仍有未安全接管的疑似凭据（{summary}）；本次未发送任何内容"
        )));
    }
    Ok(out)
}

fn high_confidence_summary(content: &str) -> Option<String> {
    let findings = detect_secrets(content);
    let high: Vec<&leveler_core::SecretFinding> = findings
        .iter()
        .filter(|finding| finding.level == SecretLevel::HighConfidence)
        .collect();
    if high.is_empty() {
        return None;
    }
    let mut labels: Vec<String> = high
        .iter()
        .map(|finding| match finding.key.as_deref() {
            Some(key) => format!(
                "{key} {}",
                leveler_core::masked_secret(finding.value(content))
            ),
            None => leveler_core::masked_secret(finding.value(content)),
        })
        .collect();
    labels.sort();
    labels.dedup();
    Some(labels.join(", "))
}

fn vault_message(error: leveler_execution::secret_vault::VaultError) -> String {
    use leveler_execution::secret_vault::VaultError;
    match error {
        VaultError::Invalid => "凭据为空或所属会话无效，未接管".to_string(),
        VaultError::TooLarge => "凭据超过 64 KiB，无法接管".to_string(),
        VaultError::TooMany => "本会话已捕获过多凭据，请先撤销不再需要的凭据".to_string(),
        VaultError::Unavailable | VaultError::Expired | VaultError::Revoked => {
            "凭据当前不可用，未接管".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_execution::secret_vault::SecretVault;

    const KEY: &str = "sk-2f8a1c9e4b7d6a5f3e2c1b0a";

    fn session() -> SessionId {
        SessionId::new("s")
    }

    #[test]
    fn a_high_confidence_credential_is_refused_without_capture_or_ack() {
        let vault = SecretVault::new();
        let error = prepare_secret_input(
            &vault,
            &session(),
            &format!("帮我配置 DEEPSEEK_API_KEY={KEY}"),
            &[],
            false,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("凭据"), "{message}");
        assert!(
            !message.contains(KEY),
            "the refusal must not echo the value"
        );
        assert_eq!(vault.len(), 0);
    }

    #[test]
    fn plaintext_acknowledgement_sends_the_text_unchanged() {
        let vault = SecretVault::new();
        let text = format!("DEEPSEEK_API_KEY={KEY}");
        let prepared = prepare_secret_input(&vault, &session(), &text, &[], true).unwrap();
        assert_eq!(prepared, text);
        assert_eq!(vault.len(), 0);
    }

    #[test]
    fn ambiguous_values_pass_through_unchanged() {
        let vault = SecretVault::new();
        let text = "API_KEY=abc123def";
        let prepared = prepare_secret_input(&vault, &session(), text, &[], false).unwrap();
        assert_eq!(prepared, text);
    }

    #[test]
    fn ordinary_code_passes_through_unchanged() {
        let vault = SecretVault::new();
        for text in [
            "let password = config.password;",
            "API_KEY=${API_KEY}",
            "帮我写一个匹配 API_KEY=sk- 的正则",
        ] {
            assert_eq!(
                prepare_secret_input(&vault, &session(), text, &[], false).unwrap(),
                text
            );
        }
    }

    #[test]
    fn a_capture_replaces_its_placeholder_and_stores_the_value() {
        let vault = SecretVault::new();
        let capture = SecretCapture::new("{{capture:0}}", KEY);
        let prepared = prepare_secret_input(
            &vault,
            &session(),
            "帮我把 {{capture:0}} 配置到 .env",
            &[capture],
            false,
        )
        .unwrap();
        assert!(!prepared.contains(KEY), "{prepared}");
        assert!(prepared.starts_with("帮我把 [secret:sec_"), "{prepared}");
        assert!(prepared.ends_with(" 配置到 .env"), "{prepared}");
        assert_eq!(vault.available_count("s"), 1);
        let reference = prepared
            .split("[secret:")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .unwrap();
        let reference = SecretRef::parse(reference).unwrap();
        assert_eq!(
            vault
                .with_value("s", &reference, |value| value.to_string())
                .unwrap(),
            KEY
        );
    }

    #[test]
    fn several_captures_are_replaced_independently() {
        let vault = SecretVault::new();
        let captures = vec![
            SecretCapture::new("{{a}}", KEY),
            SecretCapture::new("{{b}}", "ghp_16C7e42F292c6912E7710c838347Ae178B4a"),
        ];
        let prepared = prepare_secret_input(
            &vault,
            &session(),
            "first {{a}} second {{b}}",
            &captures,
            false,
        )
        .unwrap();
        assert!(!prepared.contains("{{"), "{prepared}");
        assert!(!prepared.contains(KEY), "{prepared}");
        assert_eq!(prepared.matches("[secret:").count(), 2);
        assert_eq!(vault.available_count("s"), 2);
    }

    #[test]
    fn a_missing_duplicate_or_empty_placeholder_is_refused_atomically() {
        let vault = SecretVault::new();
        for (content, placeholder) in [
            ("nothing here", "{{a}}"),
            ("{{a}} and {{a}}", "{{a}}"),
            ("{{a}}", ""),
        ] {
            let error = prepare_secret_input(
                &vault,
                &session(),
                content,
                &[SecretCapture::new(placeholder, KEY)],
                false,
            )
            .unwrap_err();
            assert!(!error.to_string().contains(KEY));
            assert_eq!(vault.len(), 0, "a refused message stores nothing");
        }
    }

    #[test]
    fn an_uncaptured_high_confidence_value_rolls_back_the_captures() {
        let vault = SecretVault::new();
        let error = prepare_secret_input(
            &vault,
            &session(),
            &format!("{{{{a}}}} and also API_KEY={KEY}"),
            &[SecretCapture::new("{{a}}", "hunter2-abc123")],
            false,
        )
        .unwrap_err();
        assert!(!error.to_string().contains(KEY));
        assert_eq!(
            vault.available_count("s"),
            0,
            "the first capture was revoked on refusal"
        );
    }

    #[test]
    fn too_many_captures_are_refused() {
        let vault = SecretVault::new();
        let captures: Vec<SecretCapture> = (0..=MAX_CAPTURES_PER_MESSAGE)
            .map(|i| SecretCapture::new(format!("{{{{c{i}}}}}"), "hunter2-abc123"))
            .collect();
        let content = (0..=MAX_CAPTURES_PER_MESSAGE)
            .map(|i| format!("{{{{c{i}}}}}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(prepare_secret_input(&vault, &session(), &content, &captures, false).is_err());
        assert_eq!(vault.len(), 0);
    }

    #[test]
    fn a_capture_of_a_second_identical_value_reuses_one_reference() {
        let vault = SecretVault::new();
        let captures = vec![
            SecretCapture::new("{{a}}", KEY),
            SecretCapture::new("{{b}}", KEY),
        ];
        let prepared =
            prepare_secret_input(&vault, &session(), "{{a}} {{b}}", &captures, false).unwrap();
        let refs: Vec<&str> = prepared
            .split("[secret:")
            .skip(1)
            .filter_map(|s| s.split(']').next())
            .collect();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0], refs[1], "one value, one reference");
        assert_eq!(vault.available_count("s"), 1);
    }
}
