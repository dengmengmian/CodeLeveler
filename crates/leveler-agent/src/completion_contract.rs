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
         For a \"constraint\" obligation, say so mechanically ONLY when the \
         task names the part of the tree the work may touch — \"only change \
         files under internal/window\", \"do not modify anything outside \
         cmd/\". Then set \"proof\": \"mutation_scope\" and list in \
         \"allowed_paths\" the files or directories that MAY be modified, \
         repo-relative, as the task names them. Anything the run modifies \
         outside that list will be treated as breaking the constraint.\n\
         - A constraint about BEHAVIOUR is not a file scope. \"Nothing else \
         about the report changes\", \"do not break existing callers\" and \
         \"existing tests must not be modified\" say what must remain true, \
         not which files exist; they get no \"proof\" and no \
         \"allowed_paths\".\n\
         - If the task restricts the work but you cannot name the paths from \
         the user's own words, omit both. A guessed scope is worse than none.\n\
         \n\
         Group what you list. One entry per INDEPENDENT objective — something \
         the user could accept, reject or change on its own. Conditions that \
         belong to one objective (its details, constraints, and the proofs it \
         demands) go in that objective's \"conditions\", not beside it. \
         Splitting one objective into several entries does not make the task \
         stricter; it only makes the same obligation harder to account for. \
         Every condition still has to hold.\n\
         \n\
         Answer with ONLY a JSON object, no prose around it:\n\
         {{\n\
           \"requirements\": [\n\
             {{\"text\": \"...\", \"kind\": \"behavior\", \"conditions\": [\n\
               {{\"text\": \"...\", \"kind\": \"verification\", \"proof\": \"command_success\", \"commands\": [\"go test ./...\"]}},\n\
               {{\"text\": \"...\", \"kind\": \"constraint\", \"proof\": \"mutation_scope\", \"allowed_paths\": [\"internal/window/\"]}}\n\
             ]}}\n\
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
    /// Paths a `mutation_scope` constraint allows the work to touch. Only ever
    /// what the task itself named — the derivation is forbidden to invent one,
    /// and an empty list is not a scope.
    #[serde(default)]
    allowed_paths: Vec<String>,
    /// Conditions inside this objective, when the model nests them.
    #[serde(default)]
    conditions: Vec<RawRequirement>,
    /// Which objective this item belongs to, when the model lists items flat
    /// instead. Two shapes, one topology: the same goal is not allowed to
    /// produce five sibling objectives one run and one objective the next
    /// purely because the model chose to phrase it differently.
    #[serde(default)]
    objective: String,
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
    // A boundary on which files may change is the one obligation the runtime
    // can settle from its own record, so it is written down as a mechanical
    // condition rather than left for a reading of the work (F6). Only when the
    // task named the paths: a constraint the derivation cannot ground stays
    // semantic, because a scope invented here would be enforced as if the user
    // had asked for it.
    if kind == RequirementKind::Constraint {
        if !raw.proof.trim().eq_ignore_ascii_case("mutation_scope") {
            return None;
        }
        let allowed: Vec<String> = raw
            .allowed_paths
            .iter()
            .map(|p| p.trim().trim_matches('`').trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        // "mutation_scope over nothing" is not a scope. Left semantic rather
        // than persisted as a policy that can never be discharged.
        return (!allowed.is_empty()).then_some(EvidencePolicy::MutationScope {
            allowed_paths: allowed,
        });
    }
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

/// Fold the derivation's answer into one canonical topology.
///
/// The model may nest conditions under an objective, or list everything flat
/// with an `objective` key saying what belongs together. Both mean the same
/// thing, and both must produce the same contract — the shape of the answer is
/// the model's phrasing, not the user's intent. Items that claim no objective
/// stand alone, because an unexplained grouping is not a grouping.
///
/// Nothing is dropped: every raw item becomes an objective or a condition of
/// one. Merging obligations away would be worse than the fragmentation this
/// exists to fix.
fn canonicalize(raw: Vec<RawRequirement>) -> Vec<CompletionRequirement> {
    let mut objectives: Vec<(String, RawRequirement, Vec<RawRequirement>)> = Vec::new();
    for item in raw.into_iter().filter(|r| !r.text.trim().is_empty()) {
        let key = item.objective.trim().to_string();
        // A flat item naming an objective joins it; the first item to claim a
        // key is that objective, the rest become its conditions.
        if !key.is_empty()
            && let Some(existing) = objectives.iter_mut().find(|(k, _, _)| k == &key)
        {
            existing.2.push(item);
            continue;
        }
        let mut item = item;
        let nested = std::mem::take(&mut item.conditions);
        objectives.push((key, item, nested));
    }
    objectives
        .into_iter()
        .enumerate()
        .map(|(i, (_, parent, conditions))| {
            let id = format!("R{}", i + 1);
            let kind = kind_from(&parent.kind);
            CompletionRequirement {
                text: parent.text.trim().to_string(),
                kind,
                source: RequirementSource::OriginalGoal,
                status: RequirementStatus::Pending,
                evidence_policy: policy_from(&parent, kind),
                evidence: Vec::new(),
                acceptance_facets: conditions
                    .into_iter()
                    .enumerate()
                    .map(|(j, c)| {
                        let kind = kind_from(&c.kind);
                        AcceptanceFacet {
                            id: format!("{id}.F{}", j + 1),
                            text: c.text.trim().to_string(),
                            kind,
                            status: RequirementStatus::Pending,
                            evidence_policy: policy_from(&c, kind),
                            evidence: Vec::new(),
                        }
                    })
                    .collect(),
                id,
            }
        })
        .collect()
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
    let requirements = canonicalize(raw.requirements);
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
    AcceptanceFacet, CommandMode, CompletionContract, CompletionRequirement, EvidencePolicy,
    RequirementKind, RequirementSource, RequirementStatus,
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
            allowed_paths: Vec::new(),
            conditions: Vec::new(),
            objective: String::new(),
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

#[cfg(test)]
mod granularity_tests {
    use super::*;

    fn item(text: &str, kind: &str) -> RawRequirement {
        RawRequirement {
            text: text.into(),
            kind: kind.into(),
            proof: String::new(),
            commands: Vec::new(),
            allowed_paths: Vec::new(),
            conditions: Vec::new(),
            objective: String::new(),
        }
    }

    fn under(objective: &str, text: &str, kind: &str) -> RawRequirement {
        RawRequirement {
            objective: objective.into(),
            ..item(text, kind)
        }
    }

    /// The shape the derivation is asked for: one objective carrying its own
    /// conditions.
    #[test]
    fn nested_conditions_stay_inside_their_objective() {
        let mut parent = item("add the --stats flag", "behavior");
        parent.conditions = vec![
            item("prints the record count", "behavior"),
            item("prints the invalid count", "behavior"),
            item("leaves default output unchanged", "constraint"),
        ];
        let out = canonicalize(vec![parent]);
        assert_eq!(out.len(), 1, "one objective, not four");
        assert_eq!(out[0].acceptance_facets.len(), 3);
        assert_eq!(out[0].id, "R1");
        assert_eq!(out[0].acceptance_facets[0].id, "R1.F1");
    }

    /// The same intent listed flat, tied together by an objective key, has to
    /// produce the same contract. The topology is the user's, not the model's
    /// phrasing — a goal cannot owe five obligations one run and one the next.
    #[test]
    fn a_flat_answer_with_grouping_canonicalizes_to_the_same_topology() {
        let flat = vec![
            under("stats", "add the --stats flag", "behavior"),
            under("stats", "prints the record count", "behavior"),
            under("stats", "prints the invalid count", "behavior"),
            under("stats", "leaves default output unchanged", "constraint"),
        ];
        let out = canonicalize(flat);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].acceptance_facets.len(), 3);
        assert_eq!(out[0].text, "add the --stats flag");
    }

    /// Genuinely separate goals stay separate. Grouping is for conditions of
    /// one objective, never a way to fold two asks into one.
    #[test]
    fn independent_objectives_are_not_merged() {
        let out = canonicalize(vec![
            item("add the --stats flag", "behavior"),
            item("add a separate --json export", "behavior"),
        ]);
        assert_eq!(out.len(), 2);
    }

    /// Nearby items with no stated grouping are not assumed to belong
    /// together: an unexplained grouping is not a grouping.
    #[test]
    fn ungrouped_items_are_not_merged_by_proximity() {
        let out = canonicalize(vec![
            under("", "first ask", "behavior"),
            under("", "second ask", "behavior"),
        ]);
        assert_eq!(out.len(), 2);
    }

    /// Nothing may be lost on the way in. Folding obligations away would be a
    /// worse failure than the fragmentation this exists to fix.
    #[test]
    fn every_raw_obligation_lands_somewhere() {
        let mut parent = item("objective", "behavior");
        parent.conditions = vec![item("c1", "behavior"), item("c2", "behavior")];
        let flat = vec![
            parent,
            under("other", "second objective", "behavior"),
            under("other", "its condition", "constraint"),
            item("standalone", "behavior"),
        ];
        let raw_count = 1 + 2 + 2 + 1;
        let out = canonicalize(flat);
        let landed: usize =
            out.len() + out.iter().map(|r| r.acceptance_facets.len()).sum::<usize>();
        assert_eq!(landed, raw_count, "no obligation may vanish: {out:#?}");
    }

    /// A proof obligation keeps its standard wherever it sits.
    #[test]
    fn a_condition_carries_its_own_proof_standard() {
        let mut parent = item("fix the boundary rule", "behavior");
        parent.conditions = vec![
            RawRequirement {
                proof: "test_coverage".into(),
                ..item("the rule is covered by a test", "verification")
            },
            RawRequirement {
                proof: "command_success".into(),
                commands: vec!["`go test ./...`".into()],
                ..item("`go test ./...` must pass", "verification")
            },
        ];
        let out = canonicalize(vec![parent]);
        let facets = &out[0].acceptance_facets;
        assert_eq!(
            facets[0].evidence_policy,
            Some(EvidencePolicy::TestCoverage)
        );
        match facets[1].evidence_policy.as_ref() {
            Some(EvidencePolicy::CommandSuccess { commands, mode }) => {
                assert_eq!(*mode, CommandMode::All);
                assert_eq!(commands.len(), 1);
                assert!(
                    !commands[0].contains('`'),
                    "markdown must not reach the fingerprint"
                );
            }
            other => panic!("expected CommandSuccess, got {other:?}"),
        }
    }

    /// An objective may itself be the command obligation when the user states
    /// it standalone — grouping is not forced.
    #[test]
    fn a_standalone_command_objective_keeps_its_policy() {
        let out = canonicalize(vec![RawRequirement {
            proof: "command_success".into(),
            commands: vec!["go build ./...".into(), "go test ./...".into()],
            ..item(
                "`go build ./...` and `go test ./...` must pass",
                "verification",
            )
        }]);
        assert_eq!(out.len(), 1);
        match out[0].evidence_policy.as_ref() {
            Some(EvidencePolicy::CommandSuccess { commands, mode }) => {
                assert_eq!(*mode, CommandMode::All);
                assert_eq!(commands.len(), 2);
            }
            other => panic!("expected CommandSuccess, got {other:?}"),
        }
    }

    /// A constraint that names where the work may happen becomes a condition
    /// the runtime can settle from its own mutation record — not a sentence
    /// for the completion judge to have an opinion about (F6).
    #[test]
    fn a_named_file_scope_becomes_a_mechanical_policy() {
        let out = canonicalize(vec![RawRequirement {
            proof: "mutation_scope".into(),
            allowed_paths: vec!["internal/window/".into(), "`cmd/telemetryd/`".into()],
            ..item(
                "do not modify files outside internal/window and cmd/telemetryd",
                "constraint",
            )
        }]);
        match out[0].evidence_policy.as_ref() {
            Some(EvidencePolicy::MutationScope { allowed_paths }) => {
                assert_eq!(allowed_paths, &["internal/window/", "cmd/telemetryd/"]);
            }
            other => panic!("expected MutationScope, got {other:?}"),
        }
    }

    /// THE RUN 07 DISTINCTION. "Nothing else about the report changes" is a
    /// statement about behaviour, and it was never a file-scope restriction —
    /// reading it as one would retroactively invent an obligation the user did
    /// not write. Without a scope the derivation could name, the obligation
    /// stays semantic and nothing mechanical is enforced against it.
    #[test]
    fn a_behavioural_constraint_is_not_turned_into_a_file_scope() {
        let out = canonicalize(vec![
            item("nothing else about the report changes", "constraint"),
            item("do not break existing callers", "constraint"),
        ]);
        assert!(
            out.iter().all(|r| r.evidence_policy.is_none()),
            "behavioural constraints carry no mechanical policy: {out:?}"
        );
    }

    /// A scope the task never named is not a scope. Rather than persist a
    /// policy that permits nothing — and so could never be discharged — the
    /// obligation is left where it was, semantic.
    #[test]
    fn a_scope_without_paths_is_not_invented() {
        let out = canonicalize(vec![RawRequirement {
            proof: "mutation_scope".into(),
            allowed_paths: vec!["  ".into()],
            ..item("keep the change contained", "constraint")
        }]);
        assert!(out[0].evidence_policy.is_none());
    }

    /// A scope declared on a condition inside an objective is carried the same
    /// way an objective's own policy is.
    #[test]
    fn a_condition_can_carry_its_own_scope() {
        let out = canonicalize(vec![RawRequirement {
            conditions: vec![RawRequirement {
                proof: "mutation_scope".into(),
                allowed_paths: vec!["internal/window/".into()],
                ..item("only files under internal/window may change", "constraint")
            }],
            ..item("fix the boundary rule", "behavior")
        }]);
        assert!(matches!(
            out[0].acceptance_facets[0].evidence_policy.as_ref(),
            Some(EvidencePolicy::MutationScope { .. })
        ));
    }
}
