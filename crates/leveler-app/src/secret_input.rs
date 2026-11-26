//! Turn captured credentials into a message the runtime can safely admit.
//!
//! This is the single admission transform for user text. It does three things
//! and nothing else:
//!
//! 1. Replaces each client placeholder with an opaque `[secret:sec_…]` token
//!    after taking custody of the value in the host vault.
//! 2. Refuses text that still carries a credential candidate — high-confidence
//!    or ambiguous — without the person's capture or explicit plaintext
//!    acknowledgement.
//! 3. Records the dotenv targets and explicit field assignments the person's
//!    own message names against every reference it mentions. That is the use
//!    authorization the broker enforces later; the model cannot widen it.
//!
//! Detection never rewrites text by itself: ordinary code is not touched, and
//! a plaintext acknowledgement returns the text exactly as typed. Refusal is
//! the only automatic outcome for an unacknowledged candidate, because the
//! alternative — silently sending it to a provider — is exactly the accident
//! this module exists to prevent.

use leveler_client_protocol::ClientError;
use leveler_client_protocol::SessionId;
use leveler_client_protocol::secret::SecretCapture;
use leveler_core::{SecretLevel, detect_secrets, masked_secret};
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
    // A capture beside it is a different decision for the same bytes, so the
    // combination is malformed rather than one silently winning.
    if plaintext_ack {
        if !captures.is_empty() {
            return Err(ClientError::Runtime(
                "不能同时选择安全接管和明文发送".to_string(),
            ));
        }
        authorize_referenced_uses(vault, session_id.as_str(), content);
        return Ok(content.to_string());
    }
    if captures.len() > MAX_CAPTURES_PER_MESSAGE {
        return Err(ClientError::Runtime(format!(
            "一条消息最多接管 {MAX_CAPTURES_PER_MESSAGE} 个凭据"
        )));
    }
    if captures.is_empty() {
        return match unadmitted_summary(content) {
            None => {
                authorize_referenced_uses(vault, session_id.as_str(), content);
                Ok(content.to_string())
            }
            Some(summary) => Err(ClientError::Runtime(format!(
                "消息中检测到疑似凭据（{summary}），但尚未安全接管。请确认后重试：                 在 TUI 中选择“安全接管”，或先用 /secret 粘贴凭据；                 如确需以明文发送，请在 TUI 中明确选择。"
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
    // carries an unadmitted credential candidate — high-confidence OR
    // ambiguous — nothing is admitted. Ambiguity is a decision the person
    // makes in the review; a client that never showed one has not made it.
    if let Some(summary) = unadmitted_summary(&out) {
        for created in &created {
            vault.revoke(session, created);
        }
        return Err(ClientError::Runtime(format!(
            "消息中仍有未安全接管的疑似凭据（{summary}）；本次未发送任何内容"
        )));
    }
    // The admitted message is the user's own statement of what this capture
    // may be used for: the dotenv targets and explicit field assignments it
    // names. This runs only after every refusal check, so a rejected message
    // authorizes nothing.
    authorize_referenced_uses(vault, session, &out);
    Ok(out)
}

/// Apply the user's declared targets and fields to every reference this
/// admitted message mentions. Unknown and foreign references are ignored: the
/// vault answers `false` and learns nothing.
fn authorize_referenced_uses(vault: &SecretVault, session: &str, content: &str) {
    let targets = leveler_core::dotenv_targets_in(content);
    let fields = leveler_core::dotenv_fields_in(content);
    if targets.is_empty() && fields.is_empty() {
        return;
    }
    let mut seen: Vec<SecretRef> = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = content[cursor..].find("[secret:") {
        let start = cursor + relative;
        let Some(close) = content[start..].find(']') else {
            break;
        };
        let end = start + close + 1;
        if let Some(reference) = leveler_core::parse_secret_reference_token(&content[start..end])
            && let Ok(reference) = SecretRef::parse(reference)
            && !seen.contains(&reference)
        {
            vault.authorize_use(session, &reference, &targets, &fields);
            seen.push(reference);
        }
        cursor = end;
    }
}

/// Refuse a task submitted through a channel that has no credential review.
///
/// A one-shot CLI invocation cannot show the TUI review, so it cannot record a
/// person's decision about a detected candidate. `allow_plaintext` is the
/// explicit opt-in for sending such text as visible plaintext; without it the
/// run does not start and nothing reaches a provider.
pub fn admit_uninteractive_input(content: &str, allow_plaintext: bool) -> Result<(), String> {
    if allow_plaintext {
        return Ok(());
    }
    match unadmitted_summary(content) {
        None => Ok(()),
        Some(summary) => Err(format!(
            "检测到疑似凭据（{summary}）。CLI 运行没有交互式凭据审查，本次未发送。\
             请使用 TUI 的安全接管或 /secret；只有在你明确要以明文发送时，\
             才加 --allow-plaintext-secrets。"
        )),
    }
}

/// Describe every credential candidate the message still carries, without
/// echoing a value. High-confidence and ambiguous are both named: the person
/// must decide either way, and the level is what the review is for.
fn unadmitted_summary(content: &str) -> Option<String> {
    let findings = detect_secrets(content);
    if findings.is_empty() {
        return None;
    }
    let mut labels: Vec<String> = findings
        .iter()
        .map(|finding| {
            let level = match finding.level {
                SecretLevel::HighConfidence => "高置信",
                SecretLevel::Ambiguous => "疑似",
            };
            let masked = masked_secret(finding.value(content));
            match finding.key.as_deref() {
                Some(key) => format!("{level} {key} {masked}"),
                None => format!("{level} {masked}"),
            }
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

    fn references(prepared: &str) -> Vec<SecretRef> {
        prepared
            .split("[secret:")
            .skip(1)
            .filter_map(|rest| rest.split(']').next())
            .filter_map(|reference| SecretRef::parse(reference).ok())
            .collect()
    }

    #[test]
    fn a_bare_key_handoff_requires_a_decision() {
        let vault = SecretVault::new();
        let token = "mgzk1gr8k7bfm6z6t8ft6tdxd8ydmm5t";
        let content = format!("请用 key:{token} 继续跑测试");
        let error = prepare_secret_input(&vault, &session(), &content, &[], false)
            .expect_err("an unadmitted key hand-off must be refused");
        let text = format!("{error}");
        assert!(text.contains("疑似凭据"), "{text}");
        let acknowledged = prepare_secret_input(&vault, &session(), &content, &[], true)
            .expect("the explicit plaintext choice admits it unchanged");
        assert_eq!(acknowledged, content);
    }

    #[test]
    fn documentation_placeholders_are_not_refused() {
        let vault = SecretVault::new();
        let content =
            "Authorization: Bearer <YOUR_API_TOKEN>\n请将 <YOUR_API_TOKEN> 替换为实际 Token";
        let prepared = prepare_secret_input(&vault, &session(), content, &[], false)
            .expect("a placeholder is a shape, not a credential");
        assert_eq!(prepared, content);
    }

    #[test]
    fn a_message_that_names_a_target_and_field_bounds_the_capture() {
        let vault = SecretVault::new();
        let prepared = prepare_secret_input(
            &vault,
            &session(),
            "把 DEMO_API_KEY={{capture:0}} 写入 .env，然后继续",
            &[SecretCapture::new("{{capture:0}}", KEY)],
            false,
        )
        .unwrap();
        let references = references(&prepared);
        assert_eq!(references.len(), 1);
        let scope = vault.use_scope("s", &references[0]).unwrap();
        assert_eq!(scope.targets, vec![".env".to_string()]);
        assert_eq!(scope.fields, vec!["DEMO_API_KEY".to_string()]);
    }

    #[test]
    fn a_later_message_with_the_reference_extends_the_authorized_use() {
        let vault = SecretVault::new();
        let reference = vault.capture("s", KEY.to_string(), None).expect("capture");
        let content = format!("请把它写到 .env.local\n[secret:{}]", reference.as_str());
        let prepared = prepare_secret_input(&vault, &session(), &content, &[], false).unwrap();
        assert_eq!(prepared, content, "ordinary text is not rewritten");
        let scope = vault.use_scope("s", &reference).unwrap();
        assert_eq!(scope.targets, vec![".env.local".to_string()]);
        assert!(scope.fields.is_empty());
    }

    #[test]
    fn a_reference_in_an_acknowledged_message_still_bounds_the_capture() {
        let vault = SecretVault::new();
        let reference = vault.capture("s", KEY.to_string(), None).expect("capture");
        let content = format!("写到 .env.production：[secret:{}]", reference.as_str());
        let prepared = prepare_secret_input(&vault, &session(), &content, &[], true).unwrap();
        assert_eq!(prepared, content);
        let scope = vault.use_scope("s", &reference).unwrap();
        assert_eq!(scope.targets, vec![".env.production".to_string()]);
    }

    #[test]
    fn an_acknowledged_message_cannot_also_carry_captures() {
        let vault = SecretVault::new();
        let error = prepare_secret_input(
            &vault,
            &session(),
            "{{a}}",
            &[SecretCapture::new("{{a}}", KEY)],
            true,
        )
        .expect_err("plaintext and capture are different decisions");
        assert!(!error.to_string().contains(KEY));
        assert_eq!(vault.len(), 0);
    }

    #[test]
    fn a_forged_reference_in_the_text_grants_nothing() {
        let vault = SecretVault::new();
        let forged = "sec_00000000000000000000000000000000";
        let content = format!("把 [secret:{forged}] 写到 .env");
        let prepared = prepare_secret_input(&vault, &session(), &content, &[], false).unwrap();
        assert_eq!(prepared, content);
        let reference = SecretRef::parse(forged).unwrap();
        assert!(vault.use_scope("s", &reference).is_err());
    }

    #[test]
    fn a_channel_without_review_refuses_every_candidate_unless_told_otherwise() {
        for text in [
            format!("配置 DEEPSEEK_API_KEY={KEY}"),
            "配置密码 API_KEY=abc123def".to_string(),
        ] {
            let error = admit_uninteractive_input(&text, false)
                .expect_err("no review surface means no silent send");
            assert!(!error.contains(KEY), "{error}");
            assert!(!error.contains("abc123def"), "{error}");
            assert!(error.contains("--allow-plaintext-secrets"), "{error}");
            assert!(admit_uninteractive_input(&text, true).is_ok());
        }
        assert!(admit_uninteractive_input("帮我写一个排序函数", false).is_ok());
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
    fn ambiguous_values_are_not_sent_without_a_persons_decision() {
        // A client that shows no review cannot prove the person saw the risk.
        // The safe outcome is a refusal with a way forward, not a silent send.
        let vault = SecretVault::new();
        let error = prepare_secret_input(&vault, &session(), "API_KEY=abc123def", &[], false)
            .expect_err("an unreviewed ambiguous value must not be admitted");
        assert!(!error.to_string().contains("abc123def"));
        assert_eq!(vault.len(), 0);
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
