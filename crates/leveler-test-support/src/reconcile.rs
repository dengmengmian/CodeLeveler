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
            r#"{"verdict":"satisfied","requirements":[{"requirement":"the requested outcome","satisfied":true,"evidence":"recorded output"}],"contradictions":[],"reason":"satisfied as stated"}"#,
        ),
        finish_reason: leveler_model::FinishReason::Stop,
        usage: leveler_model::TokenUsage::default(),
    })
}
