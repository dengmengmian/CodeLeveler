//! The V2 tier mapping, pinned at the policy layer.
//!
//! The tier names did not change, so a test that passes only a profile cannot
//! tell a v1 `request_approval` (strict) from a v2 `request_approval` (the old
//! Auto). This table passes the contract. The v2 `assisted` row is the
//! effect-oriented Auto: the host's structured effect analysis, not this
//! policy layer, is what asks for a directly irreversible operation.

use leveler_execution::{
    ApprovalPolicy, CommandView, PermissionContract, PermissionProfile, PolicyKind, Requirement,
    RiskLevel,
};

fn evaluate(
    contract: PermissionContract,
    profile: PermissionProfile,
    tool: &str,
    risk: RiskLevel,
    command: Option<(&str, &[String])>,
) -> Requirement {
    let view = command.map(|(program, args)| CommandView { program, args });
    ApprovalPolicy::default().evaluate_kind(PolicyKind::of(contract, profile), tool, risk, view)
}

#[test]
fn the_first_tier_is_the_old_auto_not_the_strict_profile() {
    // A network-class tool: the strict v1 profile asks; the old Auto does not.
    assert_eq!(
        evaluate(
            PermissionContract::V1,
            PermissionProfile::RequestApproval,
            "web_fetch",
            RiskLevel::Network,
            None
        ),
        Requirement::NeedApproval
    );
    assert_eq!(
        evaluate(
            PermissionContract::V2,
            PermissionProfile::RequestApproval,
            "web_fetch",
            RiskLevel::Network,
            None
        ),
        Requirement::Auto
    );
    // A destructive tool is refused outright only by the strict profile; the
    // first tier routes it to admission (where its own reason decides).
    assert!(!PolicyKind::LegacyRestricted.permits(RiskLevel::Destructive));
    assert!(PolicyKind::LegacyAuto.permits(RiskLevel::Destructive));
}

#[test]
fn ordinary_commands_are_auto_in_every_auto_policy() {
    let args = vec!["test".to_string()];
    for (contract, profile) in [
        (PermissionContract::V1, PermissionProfile::Assisted),
        (PermissionContract::V2, PermissionProfile::RequestApproval),
        (PermissionContract::V2, PermissionProfile::Assisted),
    ] {
        assert_eq!(
            evaluate(
                contract,
                profile,
                "shell_command",
                RiskLevel::WorkspaceWrite,
                Some(("cargo", &args))
            ),
            Requirement::Auto,
            "{contract:?} {profile:?}"
        );
    }
}

#[test]
fn dangerous_commands_keep_asking_in_the_legacy_auto_policies() {
    let args = vec!["-n".to_string(), "true".to_string()];
    for (contract, profile) in [
        (PermissionContract::V1, PermissionProfile::Assisted),
        (PermissionContract::V2, PermissionProfile::RequestApproval),
    ] {
        assert_eq!(
            evaluate(
                contract,
                profile,
                "run_command",
                RiskLevel::WorkspaceWrite,
                Some(("sudo", &args))
            ),
            Requirement::NeedApproval,
            "{contract:?} {profile:?}"
        );
    }
}

/// V2 Auto's tier-2 contract: `sudo` is an execution wrapper, not an
/// irreversible effect by itself. The host's effect analysis is the only
/// thing that turns a direct destructive operation into an ask.
#[test]
fn effect_auto_treats_a_wrapped_dangerous_shape_as_ordinary_work() {
    let args = vec!["-n".to_string(), "true".to_string()];
    assert_eq!(
        evaluate(
            PermissionContract::V2,
            PermissionProfile::Assisted,
            "run_command",
            RiskLevel::WorkspaceWrite,
            Some(("sudo", &args))
        ),
        Requirement::Auto
    );
}

#[test]
fn full_is_identical_under_both_contracts() {
    for contract in [PermissionContract::V1, PermissionContract::V2] {
        for risk in [
            RiskLevel::Safe,
            RiskLevel::WorkspaceWrite,
            RiskLevel::Network,
            RiskLevel::Destructive,
            RiskLevel::Privileged,
        ] {
            assert_eq!(
                evaluate(
                    contract,
                    PermissionProfile::FullAccess,
                    "run_command",
                    risk,
                    Some(("rm", &["-rf".to_string(), "/".to_string()]))
                ),
                Requirement::Auto
            );
        }
    }
}
