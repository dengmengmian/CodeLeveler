//! Behavioural tests for the structured user-input credential detector.
//!
//! These tests are deliberately organised as a corpus: positives, negatives
//! and ambiguous cases are separate lists, so a change that raises recall can
//! be seen immediately if it costs precision. The corpus is the versioned
//! benchmark the product gate reads.

use leveler_core::secret::{
    SecretEvidence, SecretFinding, SecretKind, SecretLevel, detect_secrets, masked_secret,
};

fn values(text: &str) -> Vec<(String, SecretLevel)> {
    detect_secrets(text)
        .iter()
        .map(|finding| (finding.value(text).to_string(), finding.level))
        .collect()
}

fn only_value(text: &str, level: SecretLevel) -> String {
    let found = values(text);
    assert_eq!(
        found.len(),
        1,
        "expected one finding for {text:?}: {found:?}"
    );
    assert_eq!(found[0].1, level, "level for {text:?}");
    found[0].0.clone()
}

fn assert_ordinary(text: &str) {
    let found = detect_secrets(text);
    assert!(
        found.is_empty(),
        "ordinary text was flagged: {text:?} -> {found:?}"
    );
}

// ── Positive corpus: formats that identify themselves ──────────────────────

#[test]
fn known_provider_keys_are_high_confidence_and_span_only_the_value() {
    let cases: &[(&str, &str)] = &[
        (
            "帮我把 DeepSeek API Key 配置到当前项目：DEEPSEEK_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a",
            "sk-2f8a1c9e4b7d6a5f3e2c1b0a",
        ),
        (
            r#"{"api_key": "sk-proj-AbCdEf0123456789xyzXYZ"}"#,
            "sk-proj-AbCdEf0123456789xyzXYZ",
        ),
        (
            "GITHUB_TOKEN=ghp_16C7e42F292c6912E7710c838347Ae178B4a",
            "ghp_16C7e42F292c6912E7710c838347Ae178B4a",
        ),
        (
            "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
        ),
        (
            "SLACK=xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx",
            "xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx",
        ),
        (
            "STRIPE=sk_live_51H8xQ2eZvKYlo2CxyzABCDEF",
            "sk_live_51H8xQ2eZvKYlo2CxyzABCDEF",
        ),
        (
            "GOOGLE=AIzaSyA1234567890abcdefghijklmnopqrstu",
            "AIzaSyA1234567890abcdefghijklmnopqrstu",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            only_value(input, SecretLevel::HighConfidence),
            *expected,
            "{input}"
        );
        let finding = &detect_secrets(input)[0];
        assert_eq!(finding.kind, SecretKind::ProviderKey, "{input}");
        assert!(
            finding.evidence.contains(&SecretEvidence::KnownPrefix),
            "{input}: {:?}",
            finding.evidence
        );
    }
}

#[test]
fn jwt_is_one_high_confidence_finding_not_two_assignments() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
    let input = format!("Authorization: Bearer {jwt}\n");
    let found = detect_secrets(&input);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].value(&input), jwt);
    assert_eq!(found[0].level, SecretLevel::HighConfidence);
    assert_eq!(found[0].kind, SecretKind::JsonWebToken);
}

#[test]
fn pem_block_is_one_finding() {
    let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdef\n-----END RSA PRIVATE KEY-----";
    let input = format!("这是私钥，请配置：\n{pem}\n后面是普通文字。");
    let found = detect_secrets(&input);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].value(&input), pem);
    assert_eq!(found[0].level, SecretLevel::HighConfidence);
    assert_eq!(found[0].kind, SecretKind::PemPrivateKey);
}

#[test]
fn url_credentials_span_the_password_only() {
    let input = "DATABASE_URL=postgres://alice:hunter2-abc123@localhost:5432/app";
    let found = detect_secrets(input);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].value(input), "hunter2-abc123");
    assert_eq!(found[0].level, SecretLevel::HighConfidence);
    assert_eq!(found[0].kind, SecretKind::UrlCredentials);
}

// ── Positive corpus: key/value positions ───────────────────────────────────

#[test]
fn credential_assignments_are_detected_in_every_supported_syntax() {
    let cases: &[(&str, &str)] = &[
        ("DATABASE_PASSWORD=hunter2-abc123", "hunter2-abc123"),
        ("export API_TOKEN=Zx81mQpL0aB7", "Zx81mQpL0aB7"),
        (r#"{"password": "hunter2-abc123"}"#, "hunter2-abc123"),
        ("client_secret: shhh-very-secret-42", "shhh-very-secret-42"),
        ("--token=Zx81mQpL0aB7", "Zx81mQpL0aB7"),
        (
            "curl -H 'X-Api-Key: Zx81mQpL0aB7' https://x",
            "Zx81mQpL0aB7",
        ),
        (
            "Authorization: Basic YWxpY2U6cGFzc3dvcmQ=",
            "YWxpY2U6cGFzc3dvcmQ=",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            only_value(input, SecretLevel::HighConfidence),
            *expected,
            "{input}"
        );
    }
}

#[test]
fn a_multiline_block_scalar_is_one_finding() {
    let input = "db_password: |\n  hunter2-abc123\n  second-line-value\nnext: keep\n";
    let found = detect_secrets(input);
    assert_eq!(found.len(), 1, "{found:?}");
    let value = found[0].value(input);
    assert!(value.starts_with("hunter2-abc123"), "{value:?}");
    assert!(value.contains("second-line-value"), "{value:?}");
    assert_eq!(found[0].level, SecretLevel::HighConfidence);
}

#[test]
fn multiple_distinct_credentials_are_returned_separately() {
    let input = "A=sk-2f8a1c9e4b7d6a5f3e2c1b0a\nB=ghp_16C7e42F292c6912E7710c838347Ae178B4a\n";
    let found = values(input);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .all(|(_, level)| *level == SecretLevel::HighConfidence)
    );
}

#[test]
fn the_same_value_twice_is_two_findings_until_the_caller_deduplicates() {
    let input = "sk-2f8a1c9e4b7d6a5f3e2c1b0a and again sk-2f8a1c9e4b7d6a5f3e2c1b0a";
    let found = detect_secrets(input);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].value(input), found[1].value(input));
}

// ── Ambiguous corpus ────────────────────────────────────────────────────────

#[test]
fn weak_credential_shaped_values_are_ambiguous_not_high() {
    for input in [
        "API_KEY=abc123def",
        "password=123456",
        "token=example123",
        "DB_PASSWORD=qwerty123",
    ] {
        assert_eq!(
            only_value(input, SecretLevel::Ambiguous),
            *input.split_once(['=', ':']).unwrap().1.trim(),
            "{input}"
        );
    }
}

#[test]
fn an_unlabelled_token_next_to_credential_words_is_ambiguous() {
    let input = "帮我把这个值配置进去：xY9pQ2vT7";
    let found = detect_secrets(input);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].value(input), "xY9pQ2vT7");
    assert_eq!(found[0].level, SecretLevel::Ambiguous);
    assert_eq!(found[0].kind, SecretKind::ProseCredential);
    assert!(found[0].evidence.contains(&SecretEvidence::ProseCue));
}

// ── Negative corpus: ordinary development material ─────────────────────────

#[test]
fn ordinary_code_and_configuration_is_not_flagged() {
    for input in [
        "const apiKey = process.env.API_KEY;",
        "let password = config.password;",
        r#"api_key = os.getenv("API_KEY")"#,
        "pub password: String,",
        "client := Config{ Password: redisPassword }",
        "if user.password.is_empty() { return Err(e); }",
        "token: Option<String>",
        "- `password: String`",
        "// TODO: validate api_key length",
        "fn validate_token(token: &str) -> bool {",
        "password == \"demo-password\"",
    ] {
        assert_ordinary(input);
    }
}

#[test]
fn templates_and_environment_references_are_not_flagged() {
    for input in [
        "API_KEY=${API_KEY}",
        "API_KEY=$API_KEY",
        "API_KEY=<YOUR_API_KEY>",
        "API_KEY=your-api-key-here",
        "API_KEY=changeme",
        "API_KEY={{ vault.api_key }}",
        "DB_PASSWORD=%DB_PASSWORD%",
        "password: [REDACTED]",
        "TOKEN=",
    ] {
        assert_ordinary(input);
    }
}

#[test]
fn status_words_and_ui_copy_are_not_flagged() {
    for input in [
        "password: required",
        "token: invalid",
        "password: true",
        r#"{"password": "Enter your password"}"#,
        "authorization: required",
        "the password field is missing",
        "请检查 password 字段是否为空",
    ] {
        assert_ordinary(input);
    }
}

#[test]
fn hashes_uuids_and_ordinary_base64_are_not_flagged() {
    for input in [
        "550e8400-e29b-41d4-a716-446655440000",
        "9fceb02d0ae598e95dc970b74767f19372d61af8",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "SGVsbG8sIHRoaXMgaXMgYSBwbGFpbiB0ZXh0IG1lc3NhZ2U=",
        "commit 9fceb02d0ae598e95dc970b74767f19372d61af8",
    ] {
        assert_ordinary(input);
    }
}

#[test]
fn paths_and_urls_are_not_flagged() {
    for input in [
        "/Users/mengmian/Develop/app/dengmengmian/CodeLeveler/crates/leveler-core/src/secret.rs",
        "https://example.com/docs?page=2",
        "see README.md#api_key for details",
    ] {
        assert_ordinary(input);
    }
}

// ── Span integrity ──────────────────────────────────────────────────────────

#[test]
fn spans_are_exact_character_boundaries_in_multibyte_text() {
    let input = "🔑 中文前缀 sk-2f8a1c9e4b7d6a5f3e2c1b0a 中文后缀";
    let found = detect_secrets(input);
    assert_eq!(found.len(), 1, "{found:?}");
    for finding in &found {
        assert!(input.is_char_boundary(finding.start));
        assert!(input.is_char_boundary(finding.end));
        assert!(finding.start < finding.end);
        assert!(finding.end <= input.len());
    }
    assert_eq!(found[0].value(input), "sk-2f8a1c9e4b7d6a5f3e2c1b0a");
}

#[test]
fn findings_are_sorted_and_never_overlap() {
    let input = "A=sk-2f8a1c9e4b7d6a5f3e2c1b0a\nB=postgres://u:hunter2-abc123@h/db\nC=ghp_16C7e42F292c6912E7710c838347Ae178B4a";
    let found = detect_secrets(input);
    assert!(found.len() >= 3, "{found:?}");
    let mut previous_end = 0;
    for finding in &found {
        assert!(finding.start >= previous_end, "overlap: {found:?}");
        previous_end = finding.end;
    }
}

#[test]
fn empty_and_whitespace_input_is_ordinary() {
    assert_ordinary("");
    assert_ordinary("   \n\t\n");
}

#[test]
fn masked_preview_never_contains_the_value() {
    let value = "sk-2f8a1c9e4b7d6a5f3e2c1b0a";
    let masked = masked_secret(value);
    assert!(!masked.contains(value), "{masked}");
    assert!(!masked.contains("sk-"), "{masked}");
    assert!(masked.contains("27"), "{masked}");
}

/// The confidence level must never be inferred from the caller's phrasing.
/// The same value in the same position has the same level regardless of the
/// sentence around it.
#[test]
fn classification_depends_on_the_value_position_not_on_prose_mood() {
    let a = "这是我的真实 Key：DEEPSEEK_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a";
    let b = "只是随便写的一个例子 DEEPSEEK_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a";
    let c = "帮我写一个匹配 DEEPSEEK_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a 的正则";
    let levels: Vec<SecretLevel> = [a, b, c]
        .iter()
        .map(|text| {
            let found = detect_secrets(text);
            assert_eq!(found.len(), 1, "{text}: {found:?}");
            found[0].level
        })
        .collect();
    assert_eq!(
        levels,
        vec![
            SecretLevel::HighConfidence,
            SecretLevel::HighConfidence,
            SecretLevel::HighConfidence
        ]
    );
}

#[test]
fn the_runtime_s_own_custody_token_is_not_a_credential() {
    let reference = "sec_0123456789abcdef0123456789abcdef";
    let token = leveler_core::secret_reference_token(reference);
    let input = format!("请用 {token} 配置到 .env");
    assert_ordinary(&input);
    assert_eq!(
        leveler_core::parse_secret_reference_token(&token),
        Some(reference)
    );
    assert!(leveler_core::parse_secret_reference_token("[secret:sec_short]").is_none());
    assert!(
        leveler_core::parse_secret_reference_token("[secret:sec_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz]")
            .is_none()
    );
}

#[test]
fn the_sanitizer_preserves_custody_reference_tokens() {
    let token = leveler_core::secret_reference_token("sec_0123456789abcdef0123456789abcdef");
    let text = format!("配置 {token} 到 .env");
    let (sanitized, found) = leveler_core::sanitize_model_visible(&text);
    assert_eq!(sanitized, text);
    assert!(found.is_empty(), "{found:?}");
    // The same token as an assignment value must also survive.
    for line in [
        format!("SECRET_REF={token}"),
        format!("TOKEN=[secret:{}]", "sec_0123456789abcdef0123456789abcdef"),
    ] {
        let (sanitized, _) = leveler_core::sanitize_model_visible(&line);
        assert!(sanitized.contains(&token), "{sanitized}");
    }
}

#[test]
fn finding_accessors_are_usable_without_exposing_values_in_debug_output() {
    let input = "API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a";
    let finding: SecretFinding = detect_secrets(input).remove(0);
    assert_eq!(finding.value(input), "sk-2f8a1c9e4b7d6a5f3e2c1b0a");
    // Debug is metadata only; the value lives in the caller's text, never in
    // the finding itself.
    let debug = format!("{finding:?}");
    assert!(!debug.contains("2f8a1c9e"), "{debug}");
}
