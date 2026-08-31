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
/// 4096 was calibrated when derivation only listed obligations. Evidence
/// Policy V1 made it also decide the proof standard and extract commands, and
/// the heavier task hit the ceiling on the real HC-002 task: measured
/// `finish=length`, `reasoning_tokens=4096/4096`, content empty — the
/// reasoning consumed the whole completion budget before a single character of
/// the object. The ceiling covers reasoning and content together, so a leaner
/// schema would not have saved it.
const MAX_OUTPUT_TOKENS: u32 = 16384;

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
         For a \"verification\" obligation, say how it can be proven:\n\
         - if the task names commands that must run or pass, list them in \
         \"commands\" exactly as written (program and arguments), and set \
         \"proof\": \"command_success\";\n\
         - if the task asks for something to be COVERED BY a test rather than \
         for a command to pass, set \"proof\": \"test_coverage\" and leave \
         \"commands\" empty. \"go test must pass\" is a command; \"the \
         boundary rule is covered by a test\" is coverage, and a suite passing \
         is not by itself that coverage.\n\
         - if neither fits, omit \"proof\".\n\
         \n\
         Answer with ONLY a JSON object, no prose around it:\n\
         {{\n\
           \"requirements\": [\n\
             {{\"text\": \"...\", \"kind\": \"behavior\", \"proof\": \"command_success\", \"commands\": [\"go test ./...\"]}}\n\
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
    #[serde(default)]
    proof: String,
    #[serde(default)]
    commands: Vec<String>,
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

/// Why derivation produced no contract.
///
/// Every one of these used to be the same `None`, so a budget exhaustion and a
/// provider outage and a malformed reply were indistinguishable in the logs —
/// diagnosing the real one took a hand-replayed API call. Product behaviour is
/// identical for all of them (no contract, and no contract cannot complete);
/// this exists so the next failure names itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DerivationFailure {
    /// The reply hit the completion budget before delivering the object.
    OutputBudgetExhausted,
    Timeout,
    ProviderError,
    /// Transport succeeded and the model said nothing.
    EmptyResponse,
    /// Content arrived carrying no decodable object.
    NoStructuredObject,
    /// An object arrived that is not the shape asked for.
    InvalidStructuredObject,
    /// Several different decodable objects — no way to tell which is meant.
    AmbiguousObjects,
    /// A well-formed answer that lists nothing. Not a contract: "no
    /// obligations found" must never come out of a derivation.
    EmptyRequirements,
}

impl DerivationFailure {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::OutputBudgetExhausted => "output_budget_exhausted",
            Self::Timeout => "timeout",
            Self::ProviderError => "provider_error",
            Self::EmptyResponse => "empty_response",
            Self::NoStructuredObject => "no_structured_object",
            Self::InvalidStructuredObject => "invalid_structured_object",
            Self::AmbiguousObjects => "ambiguous_objects",
            Self::EmptyRequirements => "empty_requirements",
        }
    }
}

/// The proof standard for an obligation, fixed here — before the work starts,
/// and so before anyone knows what evidence will happen to exist.
///
/// Only `verification` obligations carry one. An unrecognised or absent proof
/// standard is `Unresolved` rather than a guess: assuming "any green check"
/// is how an unwritten test gets waved through.
fn policy_from(raw: &RawRequirement, kind: RequirementKind) -> Option<EvidencePolicy> {
    if kind != RequirementKind::Verification {
        return None;
    }
    match raw.proof.trim().to_ascii_lowercase().as_str() {
        "command_success" if !raw.commands.is_empty() => Some(EvidencePolicy::CommandSuccess {
            commands: raw
                .commands
                .iter()
                .map(|c| {
                    // Tasks state commands in prose, and prose marks code with
                    // backticks: "`go test ./...` must pass". A fingerprint
                    // that keeps the markup matches nothing the runtime ever
                    // records, so the obligation could never be discharged.
                    // Only the markdown fence is stripped — shell quotes can
                    // carry meaning and are left alone.
                    let cleaned = c.trim().trim_matches('`').trim();
                    let mut parts = cleaned.split_whitespace().map(str::to_string);
                    let program = parts.next().unwrap_or_default();
                    let args: Vec<String> = parts.collect();
                    // Reuse the ledger's own normalization so a stated command
                    // and a recorded one are the same string.
                    leveler_lifecycle::EvidenceLedger::normalize_command_fingerprint(
                        &program, &args,
                    )
                })
                .collect(),
            // "X and Y must pass" is not satisfied by Y. Only wording that
            // actually offers a choice would justify Any, and nothing does yet.
            mode: CommandMode::All,
        }),
        "test_coverage" => Some(EvidencePolicy::TestCoverage),
        _ => Some(EvidencePolicy::Unresolved),
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
) -> Result<CompletionContract, DerivationFailure> {
    if goal.trim().is_empty() {
        return Err(DerivationFailure::EmptyRequirements);
    }
    let mut request = ModelRequest::new(
        model.clone(),
        vec![Message::text(Role::User, instruction(goal))],
    );
    request.tool_choice = ToolChoice::None;
    request.max_output_tokens = Some(MAX_OUTPUT_TOKENS);
    request.reasoning_effort = Some(DERIVE_EFFORT);
    let response = match tokio::time::timeout(
        timeout,
        runtime.generate(request, cancellation.child_token()),
    )
    .await
    {
        Err(_) => return Err(DerivationFailure::Timeout),
        Ok(Err(_)) => return Err(DerivationFailure::ProviderError),
        Ok(Ok(response)) => response,
    };
    let text = response.message.text_content();
    classify_reply(&text, response.finish_reason)?;
    // Same acceptance shape as the reconciliation gate: a bare object, a
    // fenced one, or prose around exactly one decodable object.
    let mut parsed: Option<RawContract> = None;
    let mut decodable = 0usize;
    let candidates = crate::reconciliation::candidate_objects(&text);
    for candidate in &candidates {
        if let Ok(raw) = serde_json::from_str::<RawContract>(candidate) {
            decodable += 1;
            if decodable > 1 {
                return Err(DerivationFailure::AmbiguousObjects);
            }
            parsed = Some(raw);
        }
    }
    let raw = match parsed {
        Some(raw) => raw,
        // Content arrived: either it held no object at all, or the object it
        // held was not the shape asked for.
        None if candidates.is_empty() => return Err(DerivationFailure::NoStructuredObject),
        None => return Err(DerivationFailure::InvalidStructuredObject),
    };
    let requirements: Vec<CompletionRequirement> = raw
        .requirements
        .into_iter()
        .filter(|r| !r.text.trim().is_empty())
        .enumerate()
        .map(|(i, r)| {
            let kind = kind_from(&r.kind);
            CompletionRequirement {
                id: format!("R{}", i + 1),
                text: r.text.trim().to_string(),
                kind,
                source: RequirementSource::OriginalGoal,
                status: RequirementStatus::Pending,
                evidence_policy: policy_from(&r, kind),
                evidence: Vec::new(),
            }
        })
        .collect();
    if requirements.is_empty() {
        return Err(DerivationFailure::EmptyRequirements);
    }
    Ok(CompletionContract::new(requirements))
}

/// Separate "said nothing" from "ran out of room saying it".
///
/// Both arrive as empty content, and treating them alike is what made a
/// budget ceiling look like a model that would not answer.
fn classify_reply(
    text: &str,
    finish_reason: leveler_model::FinishReason,
) -> Result<(), DerivationFailure> {
    if finish_reason == leveler_model::FinishReason::Length && text.trim().is_empty() {
        return Err(DerivationFailure::OutputBudgetExhausted);
    }
    if text.trim().is_empty() {
        return Err(DerivationFailure::EmptyResponse);
    }
    Ok(())
}

use leveler_lifecycle::{
    CommandMode, CompletionContract, CompletionRequirement, EvidencePolicy, RequirementKind,
    RequirementSource, RequirementStatus,
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
            .is_err()
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
            .is_err()
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
        assert_eq!(
            requests[0].max_output_tokens,
            Some(16384),
            "derivation needs room for reasoning AND the object it must emit"
        );
    }
}

#[cfg(test)]
mod derivation_reliability_tests {
    use super::*;
    use leveler_lifecycle::EvidenceLedger;

    /// What the ledger records when the agent actually runs the command.
    fn ledger_fingerprint(program: &str, args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        EvidenceLedger::normalize_command_fingerprint(program, &args)
    }

    fn raw(proof: &str, commands: &[&str]) -> RawRequirement {
        RawRequirement {
            text: "commands must pass".into(),
            kind: "verification".into(),
            proof: proof.into(),
            commands: commands.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn commands_of(policy: Option<EvidencePolicy>) -> Vec<String> {
        match policy {
            Some(EvidencePolicy::CommandSuccess { commands, .. }) => commands,
            other => panic!("expected CommandSuccess, got {other:?}"),
        }
    }

    /// The task states its commands in Markdown — "`go test ./...` must pass"
    /// — so the derivation is likely to hand them back with the backticks
    /// still attached. A fingerprint that keeps them matches nothing the
    /// runtime ever records, and the obligation could never be discharged.
    #[test]
    fn a_backticked_command_still_matches_what_the_runtime_records() {
        let policy = policy_from(
            &raw("command_success", &["`go test ./...`"]),
            RequirementKind::Verification,
        );
        assert_eq!(
            commands_of(policy),
            vec![ledger_fingerprint("go", &["test", "./..."])]
        );
    }

    #[test]
    fn both_backticked_commands_are_materialized() {
        let policy = policy_from(
            &raw("command_success", &["`go build ./...`", "`go test ./...`"]),
            RequirementKind::Verification,
        );
        assert_eq!(
            commands_of(policy),
            vec![
                ledger_fingerprint("go", &["build", "./..."]),
                ledger_fingerprint("go", &["test", "./..."]),
            ]
        );
    }

    /// Whatever the quoting cleanup does, it must not make different commands
    /// look alike: a suite run is not a package run.
    #[test]
    fn normalization_does_not_broaden_the_match() {
        let all = commands_of(policy_from(
            &raw("command_success", &["`go test ./...`"]),
            RequirementKind::Verification,
        ));
        let one = commands_of(policy_from(
            &raw("command_success", &["go test ./pkg/foo"]),
            RequirementKind::Verification,
        ));
        assert_ne!(all, one);
    }

    /// Plain commands are unaffected.
    #[test]
    fn an_unquoted_command_is_unchanged() {
        let policy = policy_from(
            &raw("command_success", &["go test ./..."]),
            RequirementKind::Verification,
        );
        assert_eq!(
            commands_of(policy),
            vec![ledger_fingerprint("go", &["test", "./..."])]
        );
    }

    /// The budget that the real task exhausted.
    #[test]
    fn derivation_asks_for_a_budget_that_fits_its_answer() {
        assert_eq!(MAX_OUTPUT_TOKENS, 16384);
    }

    /// Running out of room mid-answer is not the same as having nothing to
    /// say, and the difference is the whole reason this classification exists.
    #[test]
    fn an_exhausted_budget_is_named_as_such() {
        assert_eq!(
            classify_reply("", leveler_model::FinishReason::Length),
            Err(DerivationFailure::OutputBudgetExhausted)
        );
        assert_eq!(
            classify_reply("   \n", leveler_model::FinishReason::Stop),
            Err(DerivationFailure::EmptyResponse)
        );
        assert_eq!(
            classify_reply("{\"requirements\":[]}", leveler_model::FinishReason::Stop),
            Ok(())
        );
    }
}
