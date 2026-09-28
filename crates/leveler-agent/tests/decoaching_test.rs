//! Harness de-coaching: production control states contracts and facts.
//! It does not tell the model how to investigate, edit, verify, delegate, or stop.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use leveler_agent::{Executor, NoopSink};
use leveler_core::RequestId;
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, PromptAuthority, PromptSegment,
    PromptSource, Role, SegmentLifecycle, TokenUsage,
};
use leveler_tools::ToolContext;
use tokio_util::sync::CancellationToken;

struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Script {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ModelRuntime for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        Ok(answer("done"))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        let mut events = vec![Ok(ModelEvent::MessageStarted {
            request_id: response.request_id,
        })];
        for part in response.message.content {
            if let ContentPart::Text { text } = part {
                events.push(Ok(ModelEvent::TextDelta { delta: text }));
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id":model.to_string(),"provider":model.provider,"model_id":model.model,
            "protocol":"openai_chat",
            "capabilities":{"streaming":true,"tool_calling":true,"parallel_tool_calls":false,
                "structured_output":false,"reasoning":false,"vision":false},
            "limits":{"context_window":131072,"reliable_context":65536,"max_output_tokens":8192,
                "max_tool_schema_bytes":32768,"max_parallel_tool_calls":1},
            "reasoning":{"style":"none"}
        }))
        .unwrap())
    }
}

fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn executor(root: &std::path::Path, script: Arc<Script>) -> Executor {
    Executor::new(
        script,
        Arc::new(leveler_tools::default_registry()),
        ToolContext::new(Workspace::new(root).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        4,
    )
    .with_delegation(false)
}

fn block<'a>(request: &'a ModelRequest, name: &str) -> &'a PromptSegment {
    request
        .control_context
        .blocks
        .iter()
        .find(|segment| segment.name == name)
        .unwrap_or_else(|| panic!("missing segment {name}"))
}

fn absent(request: &ModelRequest, name: &str) {
    assert!(
        request
            .control_context
            .blocks
            .iter()
            .all(|segment| segment.name != name),
        "{name} must not be assembled"
    );
}

const STRATEGY: &[&str] = &[
    "keep working",
    "start coding",
    "Stop expanding",
    "Do not start new searches",
    "before other task",
    "follow completely",
    "Follow each loaded",
    "do not redo",
    "build on it",
    "re-establish",
    "Independent observations",
    "Independent actions in one turn",
    "You MUST load",
    "you should load",
    "Greeting / small talk",
    "Pure Q&A",
    "do not edit code",
    "Do not retry the same command",
];

fn assert_no_strategy(text: &str) {
    for needle in STRATEGY {
        assert!(
            !text.contains(needle),
            "production control still coaches (`{needle}`): {text}"
        );
    }
}

#[tokio::test]
async fn chat_control_has_no_goal_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let script = Script::new(vec![answer("done")]);
    executor(dir.path(), script.clone())
        .run(
            "inspect the crate",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = script.requests.lock().unwrap().remove(0);
    absent(&request, "goal_contract");
    absent(&request, "finalization");
    absent(&request, "investigation_batching");
    absent(&request, "post_edit_throughput");
    let text = request.control_context.text();
    assert!(!text.contains("GOAL MODE"), "{text}");
    assert_no_strategy(&text);
    let base = block(&request, "base");
    assert_eq!(base.authority, PromptAuthority::CoreContract);
    assert_eq!(base.source, PromptSource::BasePrompt);
    assert!(base.text.contains("language named under Turn context"));
    let delivery = block(&request, "final_delivery");
    assert_eq!(delivery.authority, PromptAuthority::CoreContract);
    assert_eq!(delivery.source, PromptSource::FinalDelivery);
    assert!(
        delivery
            .text
            .contains("An uncommitted working tree is a normal delivery state"),
        "the delivery contract still belongs to every turn"
    );
}

#[tokio::test]
async fn goal_mode_states_the_resolution_contract_only() {
    let dir = tempfile::tempdir().unwrap();
    let script = Script::new(vec![answer("done"), answer("done")]);
    executor(dir.path(), script.clone())
        .with_goal_mode(true)
        .run(
            "fix the parser",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = script.requests.lock().unwrap().remove(0);
    let goal = block(&request, "goal_contract");
    assert_eq!(goal.authority, PromptAuthority::CoreContract);
    assert_eq!(goal.source, PromptSource::GoalProtocol);
    assert_eq!(goal.lifecycle, SegmentLifecycle::SessionPrefix);
    assert!(!goal.authority_mismatch);
    assert!(goal.text.contains("update_goal(status=\"complete\")"));
    assert!(goal.text.contains("update_goal(status=\"blocked\")"));
    assert!(goal.text.contains("does not resolve the Goal"));
    assert!(
        goal.text
            .contains("must not be rewritten into an easier objective")
    );
    for banned in [
        "Greeting",
        "Q&A",
        "keep working",
        "start coding",
        "verify first",
    ] {
        assert!(
            !goal.text.contains(banned),
            "goal contract must not coach (`{banned}`): {}",
            goal.text
        );
    }
    assert_no_strategy(&request.control_context.text());
}

#[tokio::test]
async fn turn_facts_are_facts_and_language_behavior_has_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let script = Script::new(vec![answer("done")]);
    executor(dir.path(), script.clone())
        .run(
            "把这个仓库改造成生产级工具库",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = script.requests.lock().unwrap().remove(0);
    let facts = block(&request, "turn_facts");
    assert_eq!(facts.source, PromptSource::TurnFacts);
    assert_eq!(facts.authority, PromptAuthority::RuntimeFact);
    assert!(!facts.authority_mismatch);
    assert!(facts.text.contains("language: Chinese (中文)"));
    assert!(facts.text.contains("permission mode:"));
    assert!(facts.text.contains("network:"));
    for banned in [
        "Write EVERY",
        "user-visible sentence",
        "unless the user asks",
    ] {
        assert!(
            !facts.text.contains(banned),
            "turn facts must not carry the output rule (`{banned}`): {}",
            facts.text
        );
    }
    let base = block(&request, "base");
    assert_eq!(base.authority, PromptAuthority::CoreContract);
    assert_eq!(
        base.text
            .matches("language named under Turn context")
            .count(),
        1
    );
    let rules = block(&request, "operating_rules");
    assert_eq!(rules.authority, PromptAuthority::CoreContract);
    assert!(!rules.authority_mismatch);
    assert!(rules.text.contains("permission boundary") || rules.text.contains("Permission:"));
    assert!(!rules.text.contains("do not edit code"));
}

#[tokio::test]
async fn a_named_skill_stays_a_procedure_without_ordering_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".leveler/skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo procedure.\n---\nSKILL_BODY_SENTINEL\n",
    )
    .unwrap();
    let script = Script::new(vec![answer("done")]);
    executor(dir.path(), script.clone())
        .run(
            "use $demo to inspect",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = script.requests.lock().unwrap().remove(0);
    let skill = block(&request, "selected_skills");
    assert_eq!(skill.authority, PromptAuthority::UserSelectedProcedure);
    assert_eq!(
        skill.source,
        PromptSource::Skill {
            names: vec!["demo".into()]
        }
    );
    assert!(!skill.authority_mismatch);
    assert!(skill.text.contains("SKILL_BODY_SENTINEL"));
    assert!(skill.text.contains("cannot override"));
    assert!(skill.text.contains("current user request"));
    for banned in [
        "before other",
        "follow completely",
        "You MUST",
        "always obey",
    ] {
        assert!(
            !skill.text.contains(banned),
            "skill entry must not order the turn (`{banned}`): {}",
            skill.text
        );
    }
    let index = block(&request, "skill_index");
    assert_eq!(index.authority, PromptAuthority::AdvisoryContext);
    assert_eq!(index.source, PromptSource::SkillCatalog);
    assert!(!index.authority_mismatch);
    assert!(index.text.contains("load_skill(name)"));
    assert!(!index.text.contains("before related work"));
    assert!(!index.text.contains("you should load"));
    assert_no_strategy(&request.control_context.text());
}

#[tokio::test]
async fn experimental_switches_do_not_add_control_segments() {
    let dir = tempfile::tempdir().unwrap();
    let script = Script::new(vec![answer("done")]);
    executor(dir.path(), script.clone())
        .with_investigation_batching(true)
        .with_post_edit_action_throughput(leveler_agent::coding::PostEditThroughputMode::Always)
        .run(
            "inspect",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = script.requests.lock().unwrap().remove(0);
    absent(&request, "investigation_batching");
    absent(&request, "post_edit_throughput");
    assert_no_strategy(&request.control_context.text());
    assert!(request.projection.is_some(), "projection is unchanged");
}
