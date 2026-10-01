//! Deterministic prefix-cache gate (dogfood Gate A).
//!
//! The provider's prefix cache is best-effort and cannot be a gate. The wire
//! prefix is a fact, and it is the fact this gate decides on: for a session's
//! consecutive model requests, the region that should be stable (the control
//! prefix and the conversation history) must be a byte-identical prefix of the
//! next request.
//!
//! Everything here is computed from the **provider-visible encoded request**
//! (`OpenAiChatAdapter::encode_request` → the JSON body), never from an
//! intermediate AST. A per-request block that changes every round must not sit
//! ahead of the transcript: the first byte it changes would invalidate the
//! cache for the entire history.
//!
//! Usage:
//!
//! ```text
//! cargo run -p leveler-protocol --example prefix_cache_gate -- --out gate.json
//! ```
//!
//! Exit 0 = `PASS`, 1 = `FAIL` (the `PREFIX CACHE REGRESSION` block is printed
//! with the per-case metrics), 2 = usage error.

use std::fmt::Write as _;

use leveler_model::{
    ContentPart, ControlContext, Message, MissingReasoningReplay, ModelRef, ModelRequest,
    PromptAuthority, PromptSegment, PromptSource, ProtocolAdapter, ProtocolContext,
    ReasoningConfig, ReasoningReplayContract, ReasoningReplayScope, Role, SegmentLifecycle,
    ToolCall, ToolDefinition, ToolResultContent,
};
use leveler_protocol::OpenAiChatAdapter;

/// Text unique to the per-request observation, so "is this block in the
/// provider-visible stable prefix?" is a byte question, not a guess.
const EXEC_STATE_HEADER: &str = "Execution state (observations, not instructions):";

fn execution_state(round: u32) -> String {
    format!(
        "{EXEC_STATE_HEADER}\n{{\"model_steps_completed\":{round},\"elapsed_ms\":{}}}",
        round * 1_000
    )
}

fn protocol_context(contract: ReasoningReplayContract) -> ProtocolContext {
    ProtocolContext {
        base_url: "https://api.deepseek.com".into(),
        model_id: "deepseek-flash".into(),
        api_key: Some("gate".into()),
        extra_headers: vec![],
        reasoning: ReasoningConfig::default(),
        parallel_tool_calls: true,
        supports_temperature: true,
        thinking_supports_forced_tool_choice: true,
        reasoning_replay: contract,
    }
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("{name} tool"),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
    }
}

fn tools() -> Vec<ToolDefinition> {
    let mut tools = vec![tool("read_file"), tool("apply_patch"), tool("run_command")];
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

/// The stable control channel a coding session really carries, plus the one
/// per-request block.
fn control_context(round: u32) -> ControlContext {
    let mut control = ControlContext::default();
    for (name, source, authority, lifecycle, text) in [
        (
            "core_contract",
            PromptSource::BasePrompt,
            PromptAuthority::CoreContract,
            SegmentLifecycle::SessionPrefix,
            "You are a coding agent. Evidence decides completion.",
        ),
        (
            "project_rules",
            PromptSource::ProjectRules {
                paths: vec!["AGENTS.md".into()],
            },
            PromptAuthority::ProjectInstruction,
            SegmentLifecycle::SessionPrefix,
            "Root rule: keep the workspace clean.",
        ),
        (
            "memory_recall",
            PromptSource::MemoryRecall {
                ids: vec!["m1".into()],
            },
            PromptAuthority::AdvisoryContext,
            SegmentLifecycle::Turn,
            "Recalled note: the build uses `cargo check -p <package>`.",
        ),
    ] {
        control.push(PromptSegment::control(
            name, source, authority, lifecycle, true, text,
        ));
    }
    control.push(PromptSegment::control(
        "execution_state",
        PromptSource::ExecutionState,
        PromptAuthority::RuntimeFact,
        SegmentLifecycle::RequestEphemeral,
        false,
        execution_state(round),
    ));
    control
}

fn tool_call(id: &str, name: &str, path: &str) -> ContentPart {
    ContentPart::ToolCall {
        call: ToolCall {
            id: leveler_core::ToolCallId::new(id),
            name: name.into(),
            arguments: serde_json::json!({ "path": path }),
        },
    }
}

fn tool_result(id: &str, content: &str) -> Message {
    Message::from_parts(
        Role::Tool,
        vec![ContentPart::ToolResult {
            result: ToolResultContent {
                call_id: leveler_core::ToolCallId::new(id),
                content: content.into(),
                is_error: false,
            },
        }],
        None,
    )
}

fn request(
    control: ControlContext,
    tools: Vec<ToolDefinition>,
    transcript: Vec<Message>,
) -> ModelRequest {
    let mut request = ModelRequest::new(ModelRef::new("deepseek", "deepseek-flash"), transcript);
    request.control_context = control;
    request.tools = tools;
    request.tool_choice = leveler_model::ToolChoice::Auto;
    request.max_output_tokens = Some(4096);
    request.reasoning_effort = None;
    request.deadline = None;
    request
}

/// One consecutive-round comparison.
struct Case {
    name: &'static str,
    area: &'static str,
    round_n: &'static str,
    round_n1: &'static str,
    /// Leading messages that MUST be byte-identical between the rounds: the
    /// stable control prefix plus the transcript both rounds share.
    expected_stable_messages: usize,
    /// The leading control prefix message count (1 when the prefix is
    /// non-empty). The volatile block must not be inside it.
    expected_leading_control_messages: usize,
    /// A tool-surface change is a legal cache reset; `false` means the case
    /// requires the tool surface to be identical.
    tool_surface_may_change: bool,
    contract: ReasoningReplayContract,
    round_n_request: ModelRequest,
    round_n1_request: ModelRequest,
}

fn cases() -> Vec<Case> {
    let reasoning = ReasoningReplayContract::raw_field(
        ReasoningReplayScope::WhenToolsPresent,
        MissingReasoningReplay::EmptyString,
    );
    let mut out = Vec::new();

    // 1. The steady tool loop: history appends, the observation changes.
    let shared_user = Message::user_input("read src/lib.rs and report what it defines");
    out.push(Case {
        name: "steady_tool_loop",
        area: "RequestProjection / ControlContext / SegmentLifecycle",
        round_n: "n",
        round_n1: "n+1",
        expected_stable_messages: 2,
        expected_leading_control_messages: 1,
        tool_surface_may_change: false,
        contract: ReasoningReplayContract::NONE,
        round_n_request: request(control_context(1), tools(), vec![shared_user.clone()]),
        round_n1_request: request(
            control_context(2),
            tools(),
            vec![
                shared_user.clone(),
                Message::from_parts(
                    Role::Assistant,
                    vec![tool_call("c1", "read_file", "src/lib.rs")],
                    None,
                ),
                tool_result("c1", "pub fn value() -> i32 { 1 }"),
            ],
        ),
    });

    // 2. Reasoning replay: the replayed reasoning of *history* is part of the
    //    stable prefix and must not move or change between rounds.
    let _ = reasoning;
    let shared = vec![
        shared_user.clone(),
        Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: "the file defines value()".into(),
                },
                tool_call("c1", "read_file", "src/lib.rs"),
            ],
            None,
        ),
        tool_result("c1", "pub fn value() -> i32 { 1 }"),
    ];
    out.push(Case {
        name: "reasoning_replay_history_is_stable",
        area: "reasoning replay + protocol encoder",
        round_n: "n",
        round_n1: "n+1",
        expected_stable_messages: 4,
        expected_leading_control_messages: 1,
        tool_surface_may_change: false,
        contract: reasoning,
        round_n_request: request(control_context(3), tools(), shared.clone()),
        round_n1_request: request(control_context(4), tools(), {
            let mut transcript = shared.clone();
            transcript.push(Message::from_parts(
                Role::Assistant,
                vec![
                    ContentPart::Reasoning {
                        text: "checking the call sites".into(),
                    },
                    tool_call("c2", "run_command", "cargo"),
                ],
                None,
            ));
            transcript.push(tool_result("c2", "ok"));
            transcript
        }),
    });

    // 3. After a fold: the head, the model-written briefing and the retained
    //    tail are the stable prefix of the next rounds.
    let summary = Message::from_parts(
        Role::User,
        vec![ContentPart::Text {
            text: format!(
                "{} earlier work was elided; src/lib.rs defines value()",
                leveler_model::COMPACTION_BREADCRUMB_MARKER
            ),
        }],
        None,
    );
    let folded = vec![
        shared_user.clone(),
        summary,
        tool_result("c1", "pub fn value() -> i32 { 1 }"),
    ];
    out.push(Case {
        name: "compaction_then_rounds",
        area: "compaction",
        round_n: "n",
        round_n1: "n+1",
        expected_stable_messages: 4,
        expected_leading_control_messages: 1,
        tool_surface_may_change: false,
        contract: ReasoningReplayContract::NONE,
        round_n_request: request(control_context(5), tools(), folded.clone()),
        round_n1_request: request(control_context(6), tools(), {
            let mut transcript = folded.clone();
            transcript.push(Message::from_parts(
                Role::Assistant,
                vec![tool_call("c2", "apply_patch", "src/lib.rs")],
                None,
            ));
            transcript.push(tool_result("c2", "applied"));
            transcript
        }),
    });

    // 4. After a resume: the runtime notice row and the interrupted turn are
    //    history, and history stays stable.
    let resumed = vec![
        shared_user.clone(),
        Message::user(
            "this task was interrupted and is now continued",
            leveler_model::TranscriptOrigin::RuntimeNotice {
                notice: leveler_model::RuntimeNoticeKind::ChildRecovery,
            },
        ),
        Message::interrupted_response("I was about to patch src/lib.rs"),
    ];
    out.push(Case {
        name: "resume_then_rounds",
        area: "resume",
        round_n: "n",
        round_n1: "n+1",
        expected_stable_messages: 4,
        expected_leading_control_messages: 1,
        tool_surface_may_change: false,
        contract: ReasoningReplayContract::NONE,
        round_n_request: request(control_context(7), tools(), resumed.clone()),
        round_n1_request: request(control_context(8), tools(), {
            let mut transcript = resumed.clone();
            transcript.push(Message::from_parts(
                Role::Assistant,
                vec![tool_call("c2", "read_file", "src/lib.rs")],
                None,
            ));
            transcript.push(tool_result("c2", "pub fn value() -> i32 { 1 }"));
            transcript
        }),
    });

    // 5. A capability change mutates the tool surface. That IS a legal cache
    //    reset; the gate must report it as such instead of calling it a
    //    regression, and must still keep the control prefix stable.
    out.push(Case {
        name: "capability_surface_change_is_a_legal_reset",
        area: "tool definition ordering / capability surface",
        round_n: "n",
        round_n1: "n+1",
        expected_stable_messages: 1,
        expected_leading_control_messages: 1,
        tool_surface_may_change: true,
        contract: ReasoningReplayContract::NONE,
        round_n_request: request(control_context(9), tools(), vec![shared_user.clone()]),
        round_n1_request: request(
            control_context(10),
            {
                let mut tools = tools();
                tools.push(tool("browser_open"));
                tools.sort_by(|a, b| a.name.cmp(&b.name));
                tools
            },
            vec![
                shared_user.clone(),
                Message::from_parts(
                    Role::Assistant,
                    vec![tool_call("c1", "browser_open", "https://example.invalid")],
                    None,
                ),
                tool_result("c1", "<html></html>"),
            ],
        ),
    });

    out
}

/// Everything the gate reports for one case.
struct Verdict {
    case: String,
    area: String,
    round_n: String,
    round_n1: String,
    passed: bool,
    failure: Option<String>,
    /// Prompt-visible (tool schemas + messages) common prefix, in bytes.
    common_prefix_bytes: usize,
    /// Message-channel common prefix, in bytes. A legal tool-surface reset
    /// moves `common_prefix_bytes` but not this one.
    messages_common_prefix_bytes: usize,
    common_prefix_tokens: u64,
    first_divergence: usize,
    divergence_message: usize,
    divergence_segment: String,
    divergence_lifecycle: String,
    expected_stable_messages: usize,
    expected_stable_prefix_bytes: usize,
    expected_stable_prefix_tokens: u64,
    expected_stable_prefix_messages: usize,
    tool_schema_changed: bool,
    legal_reset: Option<String>,
}

/// The provider-visible prompt prefix order: tool schemas are rendered ahead of
/// the messages by the provider's chat template, so a tool change is a prefix
/// change even though `messages` comes first in the JSON object.
fn prompt_visible(tools: &serde_json::Value, messages: &serde_json::Value) -> String {
    format!(
        "tools={}\nmessages={}",
        serde_json::to_string(tools).unwrap(),
        serde_json::to_string(messages).unwrap()
    )
}

fn first_divergence(a: &str, b: &str) -> usize {
    a.bytes()
        .zip(b.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| a.len().min(b.len()))
}

/// Estimated tokens of the provider-visible content of one encoded chat
/// message, using the product's single estimator.
fn message_tokens(message: &serde_json::Value) -> u64 {
    let mut estimate = leveler_model::TokenEstimate::new();
    if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
        estimate.add_text(content);
    } else if let Some(parts) = message.get("content").and_then(|c| c.as_array()) {
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                estimate.add_text(text);
            }
        }
    }
    if let Some(reasoning) = message.get("reasoning_content").and_then(|c| c.as_str()) {
        estimate.add_text(reasoning);
    }
    if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
        for call in calls {
            if let Some(name) = call.pointer("/function/name").and_then(|v| v.as_str()) {
                estimate.add_text(name);
            }
            if let Some(args) = call.pointer("/function/arguments").and_then(|v| v.as_str()) {
                estimate.add_tool(args);
            }
        }
    }
    estimate.tokens()
}

fn messages_tokens(messages: &[serde_json::Value]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

fn evaluate(case: &Case) -> Verdict {
    let context = protocol_context(case.contract);
    let adapter = OpenAiChatAdapter::new();
    let body_n = adapter
        .encode_request(&case.round_n_request, &context, true)
        .expect("round n must encode")
        .body;
    let body_n1 = adapter
        .encode_request(&case.round_n1_request, &context, true)
        .expect("round n+1 must encode")
        .body;

    let messages_n = body_n["messages"]
        .as_array()
        .expect("messages array")
        .to_vec();
    let messages_n1 = body_n1["messages"]
        .as_array()
        .expect("messages array")
        .to_vec();

    let tool_schema_changed = body_n["tools"] != body_n1["tools"];
    let wire_n = prompt_visible(&body_n["tools"], &body_n["messages"]);
    let wire_n1 = prompt_visible(&body_n1["tools"], &body_n1["messages"]);
    let divergence = first_divergence(&wire_n, &wire_n1);

    // The message channel is measured on its own too: a tool-surface change is
    // a legal cache reset, and it must not be the excuse for history to become
    // unstable as well.
    let messages_divergence = "messages=".len()
        + first_divergence(
            &serde_json::to_string(&messages_n).unwrap(),
            &serde_json::to_string(&messages_n1).unwrap(),
        );

    // The stable region is measured on round n, so it is a real byte range of
    // an encoded request rather than a reconstruction.
    let stable_messages = messages_n
        .iter()
        .take(case.expected_stable_messages)
        .cloned()
        .collect::<Vec<_>>();
    let stable_serialized = serde_json::to_string(&stable_messages).unwrap();
    let stable_bytes = stable_serialized.len() - 2; // without the brackets
    let expected_stable_prefix_bytes = "messages=".len() + 1 + stable_bytes;

    // Round n's own bytes for the stable region: asserting the divergence is
    // past this offset is asserting the whole region is a shared prefix.
    let shared_prefix_matches = messages_n1
        .iter()
        .zip(messages_n.iter())
        .take(case.expected_stable_messages)
        .filter(|(a, b)| a == b)
        .count();

    let divergence_message = {
        let mut offset = "messages=".len() + 1;
        let mut index = 0usize;
        for message in &messages_n {
            let len = serde_json::to_string(message).unwrap().len() + 1;
            if messages_divergence < offset + len {
                break;
            }
            offset += len;
            index += 1;
        }
        index
    };
    let divergence_segment = if divergence_message < case.expected_leading_control_messages {
        "control_prefix".to_string()
    } else if divergence_message >= case.expected_stable_messages {
        "control_trailing".to_string()
    } else {
        "transcript".to_string()
    };
    let divergence_lifecycle = if divergence_segment == "control_prefix" {
        "session_prefix/turn"
    } else if divergence_segment == "control_trailing" {
        "request_ephemeral"
    } else {
        "transcript"
    }
    .to_string();

    let mut failure = None;
    let mut legal_reset = None;

    if tool_schema_changed && !case.tool_surface_may_change {
        failure = Some(
            "the tool surface changed between two rounds that declare it stable (this is a real cache reset, not an observation change)"
                .to_string(),
        );
    } else if tool_schema_changed {
        legal_reset = Some("tool_surface".to_string());
    }

    if failure.is_none() && shared_prefix_matches < case.expected_stable_messages {
        let mismatched = (0..case.expected_stable_messages)
            .find(|i| messages_n.get(*i) != messages_n1.get(*i))
            .unwrap_or(0);
        failure = Some(format!(
            "the encoded message at index {mismatched} (role={}) is not byte-identical between the rounds; \
             only {shared_prefix_matches}/{} stable message(s) matched",
            messages_n
                .get(mismatched)
                .map(|m| m["role"].as_str().unwrap_or("?").to_string())
                .unwrap_or_else(|| "?".into()),
            case.expected_stable_messages
        ));
    }

    if failure.is_none() && messages_divergence <= expected_stable_prefix_bytes {
        failure = Some(format!(
            "the wire diverges at byte {messages_divergence} of the message channel, inside the region \
             that must stay stable (ends at byte {expected_stable_prefix_bytes}, message \"{divergence_message}\", \
             segment {divergence_segment}, lifecycle {divergence_lifecycle})"
        ));
    }
    if failure.is_none() && !tool_schema_changed && divergence <= messages_divergence {
        failure = Some(format!(
            "the provider-visible request diverges at byte {divergence}, inside the region that must \
             stay stable (message channel diverges at {messages_divergence})"
        ));
    }

    // The volatile block must not be in the leading control prefix, and it must
    // really change — otherwise the case proves nothing.
    if failure.is_none() {
        let leading = messages_n
            .iter()
            .take(case.expected_leading_control_messages)
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if leading.contains(EXEC_STATE_HEADER) {
            failure = Some(
                "the leading control prefix carries the per-request execution observation"
                    .to_string(),
            );
        }
    }
    if failure.is_none() {
        let tail_n = messages_n.last().and_then(|m| m["content"].as_str());
        let tail_n1 = messages_n1.last().and_then(|m| m["content"].as_str());
        if !tail_n.is_some_and(|t| t.contains(EXEC_STATE_HEADER))
            || !tail_n1.is_some_and(|t| t.contains(EXEC_STATE_HEADER))
        {
            failure = Some(
                "the per-request observation is missing from the request: the case is vacuous"
                    .to_string(),
            );
        }
    }

    Verdict {
        case: case.name.to_string(),
        area: case.area.to_string(),
        round_n: case.round_n.to_string(),
        round_n1: case.round_n1.to_string(),
        passed: failure.is_none(),
        failure,
        common_prefix_bytes: divergence,
        messages_common_prefix_bytes: messages_divergence,
        common_prefix_tokens: messages_tokens(
            &messages_n
                .iter()
                .take(case.expected_stable_messages)
                .cloned()
                .collect::<Vec<_>>(),
        ),
        first_divergence: divergence,
        divergence_message,
        divergence_segment,
        divergence_lifecycle,
        expected_stable_messages: case.expected_stable_messages,
        expected_stable_prefix_bytes,
        expected_stable_prefix_tokens: messages_tokens(&stable_messages),
        expected_stable_prefix_messages: case.expected_stable_messages,
        tool_schema_changed,
        legal_reset,
    }
}

fn main() {
    let mut out_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out_path = args.next(),
            "--help" | "-h" => {
                println!("usage: prefix_cache_gate [--out <path>]");
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let verdicts: Vec<Verdict> = cases().iter().map(evaluate).collect();
    let failed = verdicts.iter().filter(|v| !v.passed).count();
    let passed = verdicts.len() - failed;

    let mut report = String::new();
    let _ = writeln!(
        report,
        "prefix_cache_gate: {passed}/{} case(s) passed",
        verdicts.len()
    );
    for verdict in &verdicts {
        let _ = writeln!(
            report,
            "  [{}] {} ({})\n        common_prefix_bytes={} common_prefix_tokens={} first_divergence={}\n        expected_stable={{messages:{}, bytes:{}, tokens:{}}} divergence_segment={} lifecycle={}{}",
            if verdict.passed { "PASS" } else { "FAIL" },
            verdict.case,
            verdict.area,
            verdict.common_prefix_bytes,
            verdict.common_prefix_tokens,
            verdict.first_divergence,
            verdict.expected_stable_messages,
            verdict.expected_stable_prefix_bytes,
            verdict.expected_stable_prefix_tokens,
            verdict.divergence_segment,
            verdict.divergence_lifecycle,
            verdict
                .legal_reset
                .as_ref()
                .map(|r| format!(" legal_reset={r}"))
                .unwrap_or_default(),
        );
        if let Some(failure) = &verdict.failure {
            let _ = writeln!(report, "        reason: {failure}");
        }
    }

    if failed > 0 {
        let mut block = String::new();
        let _ = writeln!(block, "PREFIX CACHE REGRESSION");
        let _ = writeln!(
            block,
            "{failed} consecutive-round case(s) break the provider-visible prefix:"
        );
        for verdict in verdicts.iter().filter(|v| !v.passed) {
            let _ = writeln!(block, "round_n: {}", verdict.round_n);
            let _ = writeln!(block, "round_n1: {}", verdict.round_n1);
            let _ = writeln!(block, "case: {}", verdict.case);
            let _ = writeln!(
                block,
                "common_prefix_bytes: {}",
                verdict.common_prefix_bytes
            );
            let _ = writeln!(
                block,
                "common_prefix_tokens: {}",
                verdict.common_prefix_tokens
            );
            let _ = writeln!(block, "first_divergence: {}", verdict.first_divergence);
            let _ = writeln!(block, "segment: {}", verdict.divergence_segment);
            let _ = writeln!(block, "lifecycle: {}", verdict.divergence_lifecycle);
            let _ = writeln!(
                block,
                "expected_stable_prefix: {} message(s), {} bytes, {} tokens",
                verdict.expected_stable_prefix_messages,
                verdict.expected_stable_prefix_bytes,
                verdict.expected_stable_prefix_tokens
            );
            if let Some(failure) = &verdict.failure {
                let _ = writeln!(block, "reason: {failure}");
            }
            let _ = writeln!(block);
        }
        eprintln!("{block}");
    }

    let json = serde_json::json!({
        "gate": "prefix_cache_stability",
        "verdict": if failed == 0 { "PASS" } else { "FAIL" },
        "cases": verdicts
            .iter()
            .map(|v| serde_json::json!({
                "case": v.case,
                "area": v.area,
                "verdict": if v.passed { "PASS" } else { "FAIL" },
                "round_n": v.round_n,
                "round_n1": v.round_n1,
                "common_prefix_bytes": v.common_prefix_bytes,
                "messages_common_prefix_bytes": v.messages_common_prefix_bytes,
                "common_prefix_tokens": v.common_prefix_tokens,
                "first_divergence": v.first_divergence,
                "divergence_message_index": v.divergence_message,
                "divergence_segment": v.divergence_segment,
                "divergence_lifecycle": v.divergence_lifecycle,
                "expected_stable_prefix": {
                    "messages": v.expected_stable_prefix_messages,
                    "bytes": v.expected_stable_prefix_bytes,
                    "tokens": v.expected_stable_prefix_tokens,
                },
                "tool_schema_changed": v.tool_schema_changed,
                "legal_reset": v.legal_reset,
                "failure": v.failure,
            }))
            .collect::<Vec<_>>(),
    });

    match out_path {
        Some(path) => match std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()) {
            Ok(()) => println!("wrote {path}"),
            Err(error) => {
                eprintln!("cannot write {path}: {error}");
                std::process::exit(2);
            }
        },
        None => println!("{}", serde_json::to_string_pretty(&json).unwrap()),
    }
    eprint!("{report}");
    if failed > 0 {
        std::process::exit(1);
    }
}
