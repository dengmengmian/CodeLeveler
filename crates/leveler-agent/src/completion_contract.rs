//! Deriving the Completion Contract from the original goal.
//!
//! One call, once, at the START of a goal — before the executor has met any
//! obstacle. That timing is the whole design: `icg-6r` fails because the
//! executor discovers the objective is unsatisfiable and then reinterprets it,
//! so the list of obligations has to be written down while the task is still
//! only the user's words. Deriving it later, in the same breath as the
//! completion claim, would put the reinterpretation and the accounting in the
//! same context and reproduce the correlation this is meant to break.
//!
//! Derivation is a DERIVED structure, never a replacement: the original goal
//! stays authoritative, and a failed derivation yields `None` rather than an
//! empty contract, so nothing can read "no requirements found" as "nothing was
//! required".

/// The gate's own request profile, for the same reason the reconciliation gate
/// has one: a thinking-flag model at the main policy's effort spends the whole
/// completion budget reasoning before it emits any content (HC002-F1).
const DERIVE_EFFORT: leveler_model::ReasoningEffort = leveler_model::ReasoningEffort::Low;
const MAX_OUTPUT_TOKENS: u32 = 4096;

fn instruction(goal: &str) -> String {
    format!(
        "You are reading a coding task to list what it OBLIGES the agent to \
         deliver. You are not planning the work and not judging anything — you \
         are writing down the acceptance criteria that already exist in the \
         user's own words.\n\
         \n\
         Rules:\n\
         - List every MATERIAL obligation the user stated. Keep the user's own \
         wording; do not soften, generalise, merge, or split requirements to \
         make them easier to satisfy.\n\
         - Include obligations that are easy to overlook because they are \
         stated in passing, especially demands for tests, checks, docs, or \
         specific commands that must pass.\n\
         - Do NOT invent obligations the user did not state, and do not add \
         standard engineering practice the user never asked for.\n\
         - Do NOT decide whether anything is done. Everything you list is \
         simply outstanding at this point.\n\
         \n\
         Classify each with \"kind\":\n\
         - \"verification\": an obligation to DEMONSTRATE something — covered \
         by a test, a command that must pass, a check that must run.\n\
         - \"deliverable\": something that must exist when the work is done.\n\
         - \"constraint\": a boundary on how the work may be done.\n\
         - \"behavior\": how the thing must behave.\n\
         - \"other\": anything else material.\n\
         \n\
         <task>\n{goal}\n</task>\n\
         \n\
         Answer with ONLY a JSON object, no prose around it:\n\
         {{\n\
           \"requirements\": [\n\
             {{\"text\": \"...\", \"kind\": \"behavior\"}}\n\
           ]\n\
         }}"
    )
}

#[derive(serde::Deserialize)]
struct RawContract {
    #[serde(default)]
    requirements: Vec<RawRequirement>,
}

#[derive(serde::Deserialize)]
struct RawRequirement {
    #[serde(default)]
    text: String,
    #[serde(default)]
    kind: String,
}

fn kind_from(raw: &str) -> RequirementKind {
    match raw.trim().to_ascii_lowercase().as_str() {
        "verification" => RequirementKind::Verification,
        "deliverable" => RequirementKind::Deliverable,
        "constraint" => RequirementKind::Constraint,
        "behavior" | "behaviour" => RequirementKind::Behavior,
        _ => RequirementKind::Other,
    }
}

/// Derive the contract for one goal. `None` means the contract is UNAVAILABLE
/// — the caller keeps today's semantic gate rather than treating an absent
/// list of obligations as an empty one.
pub(crate) async fn derive_contract(
    runtime: &dyn ModelRuntime,
    model: &ModelRef,
    timeout: std::time::Duration,
    goal: &str,
    cancellation: &CancellationToken,
) -> Option<CompletionContract> {
    if goal.trim().is_empty() {
        return None;
    }
    let mut request = ModelRequest::new(
        model.clone(),
        vec![Message::text(Role::User, instruction(goal))],
    );
    request.tool_choice = ToolChoice::None;
    request.max_output_tokens = Some(MAX_OUTPUT_TOKENS);
    request.reasoning_effort = Some(DERIVE_EFFORT);
    let response = tokio::time::timeout(
        timeout,
        runtime.generate(request, cancellation.child_token()),
    )
    .await
    .ok()?
    .ok()?;
    let text = response.message.text_content();
    // Same acceptance shape as the reconciliation gate: a bare object, a
    // fenced one, or prose around exactly one decodable object.
    let mut parsed: Option<RawContract> = None;
    for candidate in crate::reconciliation::candidate_objects(&text) {
        if let Ok(raw) = serde_json::from_str::<RawContract>(candidate) {
            if parsed.is_some() {
                return None;
            }
            parsed = Some(raw);
        }
    }
    let raw = parsed?;
    let requirements: Vec<CompletionRequirement> = raw
        .requirements
        .into_iter()
        .filter(|r| !r.text.trim().is_empty())
        .enumerate()
        .map(|(i, r)| CompletionRequirement {
            id: format!("R{}", i + 1),
            text: r.text.trim().to_string(),
            kind: kind_from(&r.kind),
            source: RequirementSource::OriginalGoal,
            status: RequirementStatus::Pending,
            evidence: Vec::new(),
        })
        .collect();
    if requirements.is_empty() {
        return None;
    }
    Some(CompletionContract::new(requirements))
}

use leveler_lifecycle::{
    CompletionContract, CompletionRequirement, RequirementKind, RequirementSource,
    RequirementStatus,
};
use leveler_model::{Message, ModelRef, ModelRequest, ModelRuntime, Role, ToolChoice};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted {
        replies: std::sync::Mutex<std::collections::VecDeque<String>>,
        requests: std::sync::Mutex<Vec<ModelRequest>>,
    }

    impl Scripted {
        fn new(replies: Vec<String>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                replies: std::sync::Mutex::new(replies.into_iter().collect()),
                requests: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn recorded_requests(&self) -> Vec<ModelRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ModelRuntime for Scripted {
        async fn generate(
            &self,
            request: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelResponse, leveler_model::ModelError> {
            self.requests.lock().unwrap().push(request);
            match self.replies.lock().unwrap().pop_front() {
                Some(text) => Ok(leveler_model::ModelResponse {
                    request_id: leveler_core::RequestId::new("r"),
                    message: Message::text(Role::Assistant, text),
                    finish_reason: leveler_model::FinishReason::Stop,
                    usage: leveler_model::TokenUsage::default(),
                }),
                None => Err(leveler_model::ModelError::new(
                    leveler_model::ModelErrorKind::Other,
                    "scripted provider failure",
                )),
            }
        }
        async fn stream(
            &self,
            _r: ModelRequest,
            _c: CancellationToken,
        ) -> Result<leveler_model::ModelEventStream, leveler_model::ModelError> {
            unimplemented!()
        }
        async fn profile(
            &self,
            _m: &ModelRef,
        ) -> Result<leveler_model::ModelProfile, leveler_model::ModelError> {
            unimplemented!()
        }
    }

    fn goal() -> &'static str {
        "Fix the window boundary rule. Acceptance: go test ./... passes, the \
         boundary rule is covered by a test, and the CLI report reflects it."
    }

    #[tokio::test]
    async fn derives_the_material_obligations_with_their_kinds() {
        let runtime = Scripted::new(vec![
            r#"{"requirements":[
                {"text":"the window boundary rule is [start, start+width)","kind":"behavior"},
                {"text":"the boundary rule is covered by a test","kind":"verification"},
                {"text":"the CLI report reflects the rule end to end","kind":"behavior"}
            ]}"#
            .to_string(),
        ]);
        let contract = derive_contract(
            runtime.as_ref(),
            &ModelRef::new("mock", "m"),
            std::time::Duration::from_secs(30),
            goal(),
            &CancellationToken::new(),
        )
        .await
        .expect("a well-formed derivation yields a contract");
        assert_eq!(contract.requirements.len(), 3);
        assert_eq!(contract.requirements[1].kind, RequirementKind::Verification);
        assert!(
            contract
                .requirements
                .iter()
                .all(|r| r.status == RequirementStatus::Pending
                    && r.source == RequirementSource::OriginalGoal),
            "every derived obligation starts pending and owned by the goal"
        );
        assert!(
            contract.requirements.iter().all(|r| !r.id.is_empty()),
            "each obligation needs a stable id to be accounted for by name"
        );
    }

    /// A derivation that cannot be parsed is UNAVAILABLE, not an empty
    /// contract. An empty contract would say "nothing is required", which is
    /// the one reading that must never come out of a failure.
    #[tokio::test]
    async fn a_malformed_derivation_is_unavailable_not_an_empty_contract() {
        let runtime = Scripted::new(vec!["I think the task is clear enough.".to_string()]);
        assert!(
            derive_contract(
                runtime.as_ref(),
                &ModelRef::new("mock", "m"),
                std::time::Duration::from_secs(30),
                goal(),
                &CancellationToken::new(),
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn a_derivation_with_no_requirements_is_unavailable() {
        let runtime = Scripted::new(vec![r#"{"requirements":[]}"#.to_string()]);
        assert!(
            derive_contract(
                runtime.as_ref(),
                &ModelRef::new("mock", "m"),
                std::time::Duration::from_secs(30),
                goal(),
                &CancellationToken::new(),
            )
            .await
            .is_none()
        );
    }

    /// Derivation must not inherit the main policy's effort: it is a reading
    /// task with a fixed schema, and a thinking-flag model at max effort spends
    /// the whole budget before emitting content (the HC002-F1 shape).
    #[tokio::test]
    async fn the_derivation_request_uses_the_bounded_profile() {
        let runtime = Scripted::new(vec![
            r#"{"requirements":[{"text":"ship it","kind":"behavior"}]}"#.to_string(),
        ]);
        derive_contract(
            runtime.as_ref(),
            &ModelRef::new("mock", "m"),
            std::time::Duration::from_secs(30),
            goal(),
            &CancellationToken::new(),
        )
        .await
        .expect("contract");
        let requests = runtime.recorded_requests();
        assert_eq!(requests.len(), 1, "derivation is ONE call, not a loop");
        assert_eq!(
            requests[0].reasoning_effort,
            Some(leveler_model::ReasoningEffort::Low)
        );
        assert_eq!(requests[0].max_output_tokens, Some(4096));
    }
}
