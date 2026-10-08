//! Retirement as a DECISION, not a broadcast.
//!
//! `ShutdownWhenIdle` is fire-and-forget: the sender learns nothing, and the
//! runtime commits to refusing new work the moment it receives the command —
//! even when it was asked by a client that turned out to be wrong about the
//! runtime being idle. One client asking for an upgrade could therefore stop
//! every other client's runtime from admitting work.
//!
//! [`RetireDecision`] is the answer to the question that has to be asked
//! instead: *may I retire you right now?* The runtime answers from its own
//! authority, and an `Accepted` answer is a commitment made atomically with
//! closing admission — never a guess the client could have made from a
//! snapshot that was already stale when it arrived.
//!
//! A `Busy` answer leaves the runtime exactly as it was: still serving, still
//! admitting work. That is the whole point. The upgrade is deferred, not
//! forced.

use serde::{Deserialize, Serialize};

use crate::RestartReason;

/// Why a client wants the runtime to retire, and which generation it believes
/// it is talking to. The generation proof matters because the decision is
/// taken by whatever process is serving the endpoint *now*: if that is not the
/// generation the client observed, the answer is
/// [`RetireDecision::GenerationChanged`] rather than a decision about a runtime
/// the client never examined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RetireRequest {
    pub reason: RestartReason,
    /// The build the client observed on the runtime it intends to replace.
    /// `matches` (not equality) is the comparison, so a dirty build recognises
    /// its own artifacts.
    pub expected_build: leveler_core::BuildIdentity,
    /// The serving process the client observed. Diagnostics and a second,
    /// independent generation witness.
    #[serde(default)]
    pub expected_pid: u32,
}

/// The runtime's authoritative answer to `may I retire you now?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RetireDecision {
    /// Nothing is owed. Admission is now closed and this runtime will exit once
    /// whatever it still holds finishes — the answer is committed atomically
    /// with closing admission, so no new work can slip in behind it.
    Accepted,
    /// Work is still owed, so the runtime was NOT changed: it is still
    /// admitting new work and still serving. The caller must not spawn a
    /// replacement and must not treat this as an error.
    Busy {
        active_turns: u32,
        active_background_tasks: u32,
    },
    /// A previous request already closed admission. The reason is the one that
    /// was recorded, so a second client can see WHICH generation change is in
    /// progress instead of inventing one.
    AlreadyRetiring { reason: Option<RestartReason> },
    /// The runtime serving this request is not the generation the caller
    /// observed. Re-observe and decide again; nothing was changed.
    GenerationChanged,
    /// This runtime has no atomic retirement (a build older than the request).
    /// Callers must NOT read this as `Busy`: it says nothing about whether work
    /// is owed, only that this runtime cannot answer the atomic question.
    Unsupported,
}

impl RetireDecision {
    /// Whether a replacement may proceed. Only `Accepted` and a previously
    /// committed `AlreadyRetiring` have closed admission.
    pub fn admits_replacement(self) -> bool {
        matches!(self, Self::Accepted | Self::AlreadyRetiring { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RetireRequest {
        RetireRequest {
            reason: RestartReason::BuildMismatch,
            expected_build: leveler_core::BuildIdentity {
                version: "1.2.3".into(),
                revision: "abc".into(),
                dirty: false,
                fingerprint: "fp".into(),
            },
            expected_pid: 42,
        }
    }

    #[test]
    fn a_deferred_upgrade_is_not_a_replacement_decision() {
        assert!(RetireDecision::Accepted.admits_replacement());
        assert!(
            RetireDecision::AlreadyRetiring {
                reason: Some(RestartReason::ConfigChanged)
            }
            .admits_replacement()
        );
        for decision in [
            RetireDecision::Busy {
                active_turns: 1,
                active_background_tasks: 0,
            },
            RetireDecision::GenerationChanged,
            RetireDecision::Unsupported,
        ] {
            assert!(
                !decision.admits_replacement(),
                "{decision:?} must not authorize a replacement"
            );
        }
    }

    #[test]
    fn the_request_round_trips_with_its_generation_proof() {
        let json = serde_json::to_string(&request()).unwrap();
        assert!(json.contains("fingerprint"));
        let back: RetireRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, request());
    }

    #[test]
    fn an_unknown_decision_tag_is_a_decode_error_not_a_default() {
        // A future decision this build cannot understand must fail loudly: the
        // alternative is silently reading "Accepted" for something that is not.
        let result: Result<RetireDecision, _> =
            serde_json::from_str(r#"{"decision":"some_future_state"}"#);
        assert!(result.is_err());
    }
}
