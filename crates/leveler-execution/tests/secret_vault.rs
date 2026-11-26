use std::time::Duration;

use leveler_execution::secret_vault::{SecretRef, SecretState, SecretVault, VaultError};

fn vault() -> SecretVault {
    SecretVault::with_ttl(Duration::from_secs(60))
}

#[test]
fn a_captured_secret_resolves_only_in_its_own_session() {
    let vault = vault();
    let reference = vault
        .capture(
            "session-a",
            "hunter2-abc123".to_string(),
            Some("API_KEY".into()),
        )
        .unwrap();

    let value = vault
        .with_value("session-a", &reference, |value| value.to_string())
        .unwrap();
    assert_eq!(value, "hunter2-abc123");

    assert!(matches!(
        vault.with_value("session-b", &reference, |value| value.to_string()),
        Err(VaultError::Unavailable)
    ));
}

#[test]
fn a_guessed_or_malformed_reference_never_resolves() {
    let vault = vault();
    vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    let guessed = SecretRef::parse("sec_00000000000000000000000000000000").unwrap();
    assert!(matches!(
        vault.with_value("s", &guessed, |value| value.to_string()),
        Err(VaultError::Unavailable)
    ));
    assert!(SecretRef::parse("sec_short").is_err());
    assert!(SecretRef::parse("hunter2-abc123").is_err());
    assert!(SecretRef::parse("sec_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
}

#[test]
fn references_are_unpredictable_and_do_not_contain_the_value() {
    let vault = vault();
    let first = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    let second = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    assert_ne!(first, second);
    assert!(!first.as_str().contains("hunter2"));
    assert_eq!(first.as_str().len(), 36);
}

#[test]
fn states_advance_from_available_to_used() {
    let vault = vault();
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    assert_eq!(
        vault.state("s", &reference).unwrap(),
        SecretState::Available
    );
    vault.mark_used("s", &reference, ".env");
    assert_eq!(vault.state("s", &reference).unwrap(), SecretState::Used);
    // A used secret stays resolvable: retrying the same apply must be
    // idempotent instead of failing once the first write succeeded.
    assert!(vault.with_value("s", &reference, |_| ()).is_ok());
}

#[test]
fn revoked_and_expired_secrets_stop_resolving() {
    let vault = SecretVault::with_ttl(Duration::from_millis(30));
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    vault.revoke("s", &reference);
    assert_eq!(vault.state("s", &reference).unwrap(), SecretState::Revoked);
    assert!(matches!(
        vault.with_value("s", &reference, |_| ()),
        Err(VaultError::Revoked)
    ));

    let expiring = vault
        .capture("s", "second-secret-42".to_string(), None)
        .unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(vault.state("s", &expiring).unwrap(), SecretState::Expired);
    assert!(matches!(
        vault.with_value("s", &expiring, |_| ()),
        Err(VaultError::Expired)
    ));
}

#[test]
fn session_revocation_and_sweep_release_every_value() {
    let vault = SecretVault::with_ttl(Duration::from_millis(20));
    vault
        .capture("a", "first-secret-42".to_string(), None)
        .unwrap();
    vault
        .capture("a", "second-secret-42".to_string(), None)
        .unwrap();
    vault
        .capture("b", "third-secret-42".to_string(), None)
        .unwrap();
    assert_eq!(vault.revoke_session("a"), 2);
    assert_eq!(vault.available_count("a"), 0);
    assert_eq!(vault.available_count("b"), 1);
    std::thread::sleep(Duration::from_millis(40));
    vault.sweep();
    assert_eq!(vault.available_count("b"), 0);
    assert_eq!(vault.len(), 0);
}

#[test]
fn capture_bounds_the_value_and_the_session() {
    let vault = vault();
    assert!(matches!(
        vault.capture("", "hunter2-abc123".to_string(), None),
        Err(VaultError::Invalid)
    ));
    assert!(matches!(
        vault.capture("s", String::new(), None),
        Err(VaultError::Invalid)
    ));
    assert!(matches!(
        vault.capture("s", "   \n\t".to_string(), None),
        Err(VaultError::Invalid)
    ));
    assert!(matches!(
        vault.capture("s", "x".repeat(64 * 1024 + 1), None),
        Err(VaultError::TooLarge)
    ));
}

#[test]
fn the_vault_never_prints_a_value() {
    let vault = vault();
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), Some("API_KEY".into()))
        .unwrap();
    let debug = format!("{vault:?}");
    assert!(!debug.contains("hunter2-abc123"), "{debug}");
    let reference_debug = format!("{reference:?}");
    assert!(!reference_debug.contains("hunter2-abc123"));
    assert!(reference_debug.starts_with("SecretRef"));
}

#[test]
fn key_hints_are_names_and_can_be_read_back_for_diagnostics() {
    let vault = vault();
    let reference = vault
        .capture(
            "s",
            "hunter2-abc123".to_string(),
            Some("DEEPSEEK_API_KEY".into()),
        )
        .unwrap();
    assert_eq!(
        vault.key_hint("s", &reference).as_deref(),
        Some("DEEPSEEK_API_KEY")
    );
    assert!(vault.key_hint("other", &reference).is_none());
}

#[test]
fn a_restarted_process_cannot_resolve_the_old_process_s_references() {
    let old = vault();
    let reference = old
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    drop(old);
    // The durable transcript keeps `[secret:sec_…]`, but the value lived only
    // in the old process. A fresh vault must answer "unavailable", never
    // resurrect it.
    let restarted = vault();
    assert!(matches!(
        restarted.with_value("s", &reference, |value| value.to_string()),
        Err(VaultError::Unavailable)
    ));
    assert_eq!(restarted.available_count("s"), 0);
}

#[test]
fn a_capture_starts_with_no_authorized_use() {
    let vault = vault();
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    let scope = vault.use_scope("s", &reference).unwrap();
    assert!(scope.targets.is_empty());
    assert!(scope.fields.is_empty());
}

#[test]
fn the_users_named_targets_and_fields_bound_a_capture() {
    let vault = vault();
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    assert!(vault.authorize_use(
        "s",
        &reference,
        &[".env".to_string()],
        &["DEMO_API_KEY".to_string()]
    ));
    let scope = vault.use_scope("s", &reference).unwrap();
    assert_eq!(scope.targets, vec![".env".to_string()]);
    assert_eq!(scope.fields, vec!["DEMO_API_KEY".to_string()]);

    // A later message may extend the same capture's declared use; it never
    // replaces what the user already authorized.
    assert!(vault.authorize_use(
        "s",
        &reference,
        &[".env.local".to_string()],
        &["OTHER_TOKEN".to_string()]
    ));
    let scope = vault.use_scope("s", &reference).unwrap();
    assert_eq!(
        scope.targets,
        vec![".env".to_string(), ".env.local".to_string()]
    );
    assert_eq!(
        scope.fields,
        vec!["DEMO_API_KEY".to_string(), "OTHER_TOKEN".to_string()]
    );
}

#[test]
fn a_foreign_unknown_revoked_or_expired_capture_cannot_gain_a_use_scope() {
    let vault = SecretVault::with_ttl(Duration::from_millis(30));
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    assert!(!vault.authorize_use(
        "other",
        &reference,
        &[".env".to_string()],
        &["KEY".to_string()]
    ));
    assert!(matches!(
        vault.use_scope("other", &reference),
        Err(VaultError::Unavailable)
    ));

    let guessed = SecretRef::parse("sec_00000000000000000000000000000000").unwrap();
    assert!(!vault.authorize_use("s", &guessed, &[".env".to_string()], &["KEY".to_string()]));

    vault.revoke("s", &reference);
    assert!(!vault.authorize_use("s", &reference, &[".env".to_string()], &["KEY".to_string()]));
    assert!(matches!(
        vault.use_scope("s", &reference),
        Err(VaultError::Revoked)
    ));

    let expiring = vault
        .capture("s", "second-secret-42".to_string(), None)
        .unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert!(!vault.authorize_use("s", &expiring, &[".env".to_string()], &["KEY".to_string()]));
    assert!(matches!(
        vault.use_scope("s", &expiring),
        Err(VaultError::Expired)
    ));
}

#[test]
fn a_denial_is_sticky_and_a_new_capture_is_the_way_back() {
    let vault = vault();
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    assert!(vault.authorize_declaration(
        "s",
        &reference,
        &leveler_core::DotenvUseDeclaration {
            targets: vec![".env".to_string()],
            fields: vec!["KEY".to_string()],
            denied_targets: vec![".env.production".to_string()],
            ..Default::default()
        }
    ));
    let scope = vault.use_scope("s", &reference).unwrap();
    assert_eq!(scope.targets, vec![".env".to_string()]);
    assert_eq!(scope.denied_targets, vec![".env.production".to_string()]);

    // A later bare mention is not an explicit override. The prohibition stays
    // attached to this capture.
    assert!(vault.authorize_use("s", &reference, &[".env.production".to_string()], &[]));
    let scope = vault.use_scope("s", &reference).unwrap();
    assert_eq!(scope.denied_targets, vec![".env.production".to_string()]);
    assert!(!scope.targets.contains(&".env.production".to_string()));

    // A new capture is a new decision and starts without the old prohibition.
    let fresh = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    assert!(vault.authorize_use("s", &fresh, &[".env.production".to_string()], &[]));
    let scope = vault.use_scope("s", &fresh).unwrap();
    assert!(scope.denied_targets.is_empty());
    assert_eq!(scope.targets, vec![".env.production".to_string()]);
}

#[test]
fn an_exact_approval_and_denial_bind_to_one_reference_and_use() {
    let vault = vault();
    let first = vault
        .capture("s", "first-secret-abc123".to_string(), None)
        .unwrap();
    let second = vault
        .capture("s", "second-secret-abc123".to_string(), None)
        .unwrap();
    assert!(vault.record_exact_approval("s", &first, ".env", "A_KEY"));
    assert!(vault.record_exact_denial("s", &second, ".env", "B_KEY"));

    let scope = vault.use_scope("s", &first).unwrap();
    assert_eq!(scope.approved_uses.len(), 1);
    assert_eq!(scope.approved_uses[0].target, ".env");
    assert_eq!(scope.approved_uses[0].field, "A_KEY");
    assert!(scope.denied_uses.is_empty());

    let scope = vault.use_scope("s", &second).unwrap();
    assert_eq!(scope.denied_uses.len(), 1);
    assert_eq!(scope.denied_uses[0].field, "B_KEY");
    assert!(scope.approved_uses.is_empty());

    // The other session cannot record or learn a decision.
    assert!(!vault.record_exact_approval("other", &first, ".env", "A_KEY"));

    // A fresh approval lifts the exact refusal; a fresh refusal lifts the
    // approval. The two sets never contain the same use.
    assert!(vault.record_exact_approval("s", &second, ".env", "B_KEY"));
    let scope = vault.use_scope("s", &second).unwrap();
    assert!(scope.denied_uses.is_empty(), "{scope:?}");
    assert_eq!(scope.approved_uses.len(), 1);

    assert!(vault.record_exact_denial("s", &second, ".env", "B_KEY"));
    let scope = vault.use_scope("s", &second).unwrap();
    assert!(scope.approved_uses.is_empty(), "{scope:?}");
    assert_eq!(scope.denied_uses.len(), 1);
}

#[test]
fn revoked_and_foreign_captures_cannot_gain_or_record_a_decision() {
    let vault = vault();
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    assert!(!vault.record_exact_approval("other", &reference, ".env", "KEY"));
    vault.revoke("s", &reference);
    assert!(!vault.record_exact_approval("s", &reference, ".env", "KEY"));
    assert!(!vault.record_exact_denial("s", &reference, ".env", "KEY"));
    assert!(matches!(
        vault.use_scope("s", &reference),
        Err(VaultError::Revoked)
    ));
}

#[test]
fn an_ambiguous_declaration_is_sticky_and_carries_no_positive_grant() {
    let vault = vault();
    let reference = vault
        .capture("s", "hunter2-abc123".to_string(), None)
        .unwrap();
    assert!(vault.authorize_declaration(
        "s",
        &reference,
        &leveler_core::DotenvUseDeclaration {
            ambiguous: true,
            ..Default::default()
        }
    ));
    let scope = vault.use_scope("s", &reference).unwrap();
    assert!(scope.ambiguous);
    assert!(scope.targets.is_empty());
    assert!(scope.fields.is_empty());

    // A later explicit use is a positive grant, but the earlier mention was
    // never resolved; the flag stays for the dimensions it still covers.
    assert!(vault.authorize_use("s", &reference, &[".env".to_string()], &[]));
    let scope = vault.use_scope("s", &reference).unwrap();
    assert!(scope.ambiguous);
    assert_eq!(scope.targets, vec![".env".to_string()]);
}
