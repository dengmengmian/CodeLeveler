//! Autopilot for the Completion Reconciliation Gate in scripted-model tests.
//!
//! The gate (ICG-6R closure) issues one extra non-streaming model request per
//! accepted goal-mode `update_goal(complete)`. Tests that script a FIFO of
//! model responses would each need one more entry — and every request-count
//! assertion would shift — for a call whose verdict is not what those tests
//! are about. Instead a mock runtime's `generate` calls this first: a gate
//! request (identified by its fixed instruction header) is answered
//! "satisfied" out of band, without consuming the scripted queue.
//!
//! Tests that exercise the gate itself script their own verdicts and must NOT
//! route through this helper (or must use a scripted-queue entry instead).

use leveler_core::RequestId;
use leveler_model::{Message, ModelRequest, ModelResponse, Role};

/// The stable sentence the gate's instruction begins with. A wording tweak in
/// the product must keep this stable or update both sides — the gate's own
/// tests fail loudly if detection breaks.
pub const RECONCILE_MARKER: &str = "completion reconciliation judge";

/// The stable opening of the Completion Contract derivation instruction. Same
/// contract as [`RECONCILE_MARKER`]: keep it stable on both sides.
pub const DERIVE_MARKER: &str = "list what it OBLIGES the agent to deliver";

/// If `request` is a Completion Contract derivation call, answer out of band
/// with a minimal one-obligation contract, so scripted tests keep their FIFO
/// and their request-count assertions.
///
/// It has to be a VALID contract, not an empty one: a missing contract refuses
/// completion by design, so an empty answer here would block every scripted
/// goal-mode test for a reason that has nothing to do with what those tests
/// are about. The single obligation is a `behavior` one, which needs no
/// mechanical evidence, and [`reconcile_autopilot`] accounts for it by id. A
/// test that wants real obligations states them with `with_completion_contract`.
pub fn derive_autopilot(request: &ModelRequest) -> Option<ModelResponse> {
    let is_derivation = request
        .messages
        .iter()
        .any(|m| m.text_content().contains(DERIVE_MARKER));
    if !is_derivation {
        return None;
    }
    Some(ModelResponse {
        request_id: RequestId::new("derive-autopilot"),
        message: Message::text(
            Role::Assistant,
            r#"{"requirements":[{"text":"complete the task as the user asked","kind":"behavior"}]}"#,
        ),
        finish_reason: leveler_model::FinishReason::Stop,
        usage: leveler_model::TokenUsage::default(),
    })
}

/// If `request` is a Completion Reconciliation Gate call, produce a canned
/// "satisfied" verdict; otherwise `None` and the caller serves its script.
pub fn reconcile_autopilot(request: &ModelRequest) -> Option<ModelResponse> {
    let is_gate = request
        .messages
        .iter()
        .any(|m| m.text_content().contains(RECONCILE_MARKER));
    if !is_gate {
        return None;
    }
    Some(ModelResponse {
        request_id: RequestId::new("reconcile-autopilot"),
        message: Message::text(
            Role::Assistant,
            r#"{"verdict":"satisfied","requirements":[{"requirement":"the requested outcome","satisfied":true,"evidence":"recorded output"}],"contradictions":[],"requirement_accounting":[{"id":"R1","satisfied":true,"evidence":"recorded output","evidence_strength":"observed"}],"reason":"satisfied as stated"}"#,
        ),
        finish_reason: leveler_model::FinishReason::Stop,
        usage: leveler_model::TokenUsage::default(),
    })
}
