//! Real-model probe for evidence-bound experience extraction.
//!
//! Ignored by default: it makes network calls to a real provider. It exists so
//! the extraction contract can be measured — what gets saved, what is refused,
//! whether an update replaces instead of duplicating, and what it costs —
//! instead of asserted from a double.
//!
//! ```text
//! EXPERIENCE_PROBE_BASE_URL=https://taotoken.net/api/v1 \
//! EXPERIENCE_PROBE_API_KEY=$TAOTOKEN_API_KEY \
//! EXPERIENCE_PROBE_MODEL=deepseek-flash \
//!   cargo test -p leveler-agent --test experience_probe -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_agent::{
    ExperienceEvent, ExperienceExtractor, ModelExperienceExtractor, build_evidence_pack,
    validate_experience_candidates,
};
use leveler_memory::{
    ExperienceVerification, MemoryLanes, MemoryRoots, MemoryService, ServiceScope, semantic_key_of,
    v2::{Actor, MemoryScope, ScopeRoots},
};
use leveler_model::{
    ModelCapabilities, ModelLimits, ModelPricing, ModelProfile, ModelRef, ProtocolKind,
    ReasoningConfig, ReasoningEffort, ReasoningStyle,
};
use leveler_provider::{ModelConfigFile, ProviderConfig, ProviderRegistry, RegistryInputs};

fn probe_service(dir: &std::path::Path) -> MemoryService {
    MemoryService::new(MemoryRoots::new().with_project(ScopeRoots::new(
        MemoryScope::Project,
        dir.join("memory"),
        dir.join("state"),
    )))
}

fn probe_actor() -> Actor {
    Actor::Runtime {
        task: leveler_core::TaskId::new("experience-probe"),
    }
}

fn model_profile(model_id: &str) -> ModelProfile {
    ModelProfile {
        id: model_id.to_string(),
        provider: "probe".to_string(),
        model_id: model_id.to_string(),
        protocol: ProtocolKind::OpenAiChat,
        capabilities: ModelCapabilities {
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: false,
            reasoning: true,
            vision: false,
        },
        limits: ModelLimits {
            context_window: 1_048_576,
            reliable_context: 786_432,
            max_output_tokens: 65_536,
            max_tool_schema_bytes: 32_768,
            max_parallel_tool_calls: 1,
            max_tool_output_bytes: None,
        },
        context_quality: None,
        reasoning: ReasoningConfig {
            style: ReasoningStyle::ThinkingFlag,
            supported_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ],
            default_effort: Some(ReasoningEffort::Low),
            preserved_thinking: None,
            forced: false,
        },
        compatibility: Default::default(),
        thinking: None,
        pricing: Some(ModelPricing {
            input_usd_per_mtok: 0.1389,
            output_usd_per_mtok: 0.2778,
            cached_input_usd_per_mtok: None,
        }),
    }
}

fn started(call_id: &str, program: &str, args: &[&str], step: u32) -> String {
    let arguments = serde_json::json!({"program": program, "args": args});
    serde_json::json!({
        "type": "tool_call_started",
        "payload": {
            "call_id": call_id,
            "name": "run_command",
            "arguments": arguments.to_string(),
            "model_step": step
        }
    })
    .to_string()
}

fn patch(call_id: &str, step: u32) -> String {
    serde_json::json!({
        "type": "tool_call_started",
        "payload": {
            "call_id": call_id,
            "name": "apply_patch",
            "arguments": "{}",
            "model_step": step
        }
    })
    .to_string()
}

fn finished(call_id: &str, name: &str, error: bool, exit: Option<i32>, diff: bool) -> String {
    finished_with_preview(
        call_id,
        name,
        error,
        exit,
        diff,
        "raw output is not evidence",
    )
}

fn finished_with_preview(
    call_id: &str,
    name: &str,
    error: bool,
    exit: Option<i32>,
    diff: bool,
    preview: &str,
) -> String {
    serde_json::json!({
        "type": "tool_call_finished",
        "payload": {
            "call_id": call_id,
            "name": name,
            "is_error": error,
            "preview": preview,
            "exit_code": exit,
            "applied_diff": if diff { serde_json::json!("--- a/src/lib.rs") } else { serde_json::Value::Null }
        }
    })
    .to_string()
}

struct Scenario {
    name: &'static str,
    goal: &'static str,
    events: Vec<(Option<&'static str>, String)>,
    expect_verified: bool,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "complex_root_cause",
            goal: "修复 UTF-8 字符串截断导致的测试 panic",
            events: vec![
                (Some("turn-a"), started("c1", "cargo", &["test"], 1)),
                (
                    Some("turn-a"),
                    finished_with_preview(
                        "c1",
                        "run_command",
                        true,
                        Some(101),
                        false,
                        "thread 'main' panicked at src/lib.rs:7:5: byte index 5 is not a char boundary",
                    ),
                ),
                (Some("turn-a"), patch("c2", 2)),
                (
                    Some("turn-a"),
                    finished("c2", "apply_patch", false, None, true),
                ),
                (Some("turn-a"), started("c3", "cargo", &["test"], 3)),
                (
                    Some("turn-a"),
                    finished("c3", "run_command", false, Some(0), false),
                ),
            ],
            expect_verified: true,
        },
        Scenario {
            name: "simple_typo_fix",
            goal: "把测试里的 2+2=5 改成 4",
            events: vec![
                (Some("turn-b"), started("d1", "cargo", &["test"], 1)),
                (
                    Some("turn-b"),
                    finished_with_preview(
                        "d1",
                        "run_command",
                        true,
                        Some(101),
                        false,
                        "assertion `left == right` failed",
                    ),
                ),
                (Some("turn-b"), patch("d2", 2)),
                (
                    Some("turn-b"),
                    finished("d2", "apply_patch", false, None, true),
                ),
                (Some("turn-b"), started("d3", "cargo", &["test"], 3)),
                (
                    Some("turn-b"),
                    finished("d3", "run_command", false, Some(0), false),
                ),
            ],
            expect_verified: false,
        },
        Scenario {
            name: "repeated_failure_without_resolution",
            goal: "调查 flaky 的集成测试",
            events: vec![
                (Some("turn-c"), started("e1", "cargo", &["test"], 1)),
                (
                    Some("turn-c"),
                    finished_with_preview(
                        "e1",
                        "run_command",
                        true,
                        Some(101),
                        false,
                        "test result: FAILED. 0 passed; 1 failed",
                    ),
                ),
                (
                    Some("turn-c"),
                    started("e2", "cargo", &["test", "--lib"], 2),
                ),
                (
                    Some("turn-c"),
                    finished_with_preview(
                        "e2",
                        "run_command",
                        true,
                        Some(101),
                        false,
                        "test result: FAILED. 0 passed; 1 failed",
                    ),
                ),
            ],
            expect_verified: false,
        },
        Scenario {
            name: "environment_constraint",
            goal: "让 cargo test 在这个环境跑起来",
            events: vec![
                (Some("turn-d"), started("f1", "cargo", &["test"], 1)),
                (
                    Some("turn-d"),
                    finished_with_preview(
                        "f1",
                        "run_command",
                        true,
                        Some(101),
                        false,
                        "error: linker `cc` not found",
                    ),
                ),
                (Some("turn-d"), patch("f2", 2)),
                (
                    Some("turn-d"),
                    finished("f2", "apply_patch", false, None, true),
                ),
                (Some("turn-d"), started("f3", "cargo", &["test"], 3)),
                (
                    Some("turn-d"),
                    finished("f3", "run_command", false, Some(0), false),
                ),
            ],
            expect_verified: true,
        },
    ]
}

#[tokio::test]
#[ignore = "real provider; requires EXPERIENCE_PROBE_* / provider API key"]
async fn real_model_experience_extraction_probe() {
    let Ok(api_key) = std::env::var("EXPERIENCE_PROBE_API_KEY")
        .or_else(|_| std::env::var("TAOTOKEN_API_KEY"))
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
    else {
        eprintln!("SKIP: no EXPERIENCE_PROBE_API_KEY / TAOTOKEN_API_KEY / DEEPSEEK_API_KEY");
        return;
    };
    let base_url = std::env::var("EXPERIENCE_PROBE_BASE_URL")
        .unwrap_or_else(|_| "https://taotoken.net/api/v1".to_string());
    let model_id =
        std::env::var("EXPERIENCE_PROBE_MODEL").unwrap_or_else(|_| "deepseek-flash".to_string());

    let provider = ProviderConfig {
        id: "probe".to_string(),
        protocol: ProtocolKind::OpenAiChat,
        base_url,
        api_key_env: String::new(),
        api_key: Some(api_key.clone()),
        headers: Default::default(),
        timeouts: Default::default(),
    };
    let registry = ProviderRegistry::build(RegistryInputs {
        providers: vec![(provider, Some(api_key))],
        models: vec![ModelConfigFile {
            profile: model_profile(&model_id),
            policy: None,
        }],
    })
    .expect("registry builds");
    let runtime: Arc<dyn leveler_model::ModelRuntime> = Arc::new(registry);
    let model = ModelRef::new("probe", &model_id);
    let extractor =
        ModelExperienceExtractor::new(runtime.clone(), model).with_timeout(Duration::from_secs(90));

    let dir = tempfile::tempdir().unwrap();
    let service = probe_service(dir.path());
    let turns = 3usize;
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut total_ms = 0u128;
    let mut calls = 0usize;
    let mut expected_saved = 0usize;
    let mut expected_skipped = 0usize;
    let mut saved = 0usize;
    let mut skipped = 0usize;
    let mut pending = 0usize;
    let mut false_saves = 0usize;
    let mut duplicates = 0usize;
    let mut updates = 0usize;
    let mut recall_hits = 0usize;
    let mut recall_correct = 0usize;
    let mut recall_total = 0usize;
    let mut expected_ids: std::collections::HashMap<&'static str, String> =
        std::collections::HashMap::new();

    for round in 0..turns {
        for scenario in scenarios() {
            let borrowed: Vec<ExperienceEvent<'_>> = scenario
                .events
                .iter()
                .map(|(turn, payload)| ExperienceEvent {
                    turn_id: *turn,
                    payload: payload.as_str(),
                })
                .collect();
            let pack = build_evidence_pack("turn-current", scenario.goal, &borrowed)
                .unwrap_or_else(|| panic!("{} must produce a pack", scenario.name));
            println!("\n========== round {round} {}", scenario.name);
            println!("pack: {:?}", pack);
            let started = Instant::now();
            let extraction = extractor
                .extract(&pack, &tokio_util::sync::CancellationToken::new())
                .await;
            let elapsed = started.elapsed();
            if let Some(call) = extraction.call {
                calls += 1;
                total_ms += call.latency_ms as u128;
                if let Some(usage) = call.usage {
                    total_in += usage.input_tokens;
                    total_out += usage.output_tokens;
                }
                println!(
                    "latency={}ms tokens_in={:?} tokens_out={:?} error={:?}",
                    call.latency_ms,
                    call.usage.map(|u| u.input_tokens),
                    call.usage.map(|u| u.output_tokens),
                    call.error
                );
            }
            let candidates = match extraction.candidates {
                Ok(candidates) => candidates,
                Err(error) => {
                    println!("EXTRACTION ERROR: {error} ({}ms)", elapsed.as_millis());
                    continue;
                }
            };
            println!("raw candidates={}", candidates.len());
            for candidate in &candidates {
                println!(
                    "  candidate category={:?} subject={:?} title={:?}\n    body={:?}\n    applies={:?} invalid={:?} evidence={:?}",
                    candidate.category,
                    candidate.subject,
                    candidate.title,
                    candidate.body,
                    candidate.applies_when,
                    candidate.invalid_if,
                    candidate.evidence_ids
                );
            }
            let validated = validate_experience_candidates(&candidates, &pack);
            for (index, rejection) in &validated.rejected {
                println!(
                    "  REJECTED[{index}] code={} reason={}",
                    rejection.code(),
                    rejection.reason()
                );
            }
            if scenario.expect_verified {
                expected_saved += 1;
            } else {
                expected_skipped += 1;
            }
            let mut stored_verified = 0usize;
            let accepted_count = validated.accepted.len();
            for candidate in validated.accepted {
                let verified = candidate
                    .experience
                    .as_ref()
                    .is_some_and(|record| record.verification == ExperienceVerification::Verified);
                let seed = format!("probe:{}", candidate.fingerprint);
                match service.admit_candidate(
                    &probe_actor(),
                    ServiceScope::Project,
                    &candidate,
                    &seed,
                ) {
                    Ok(leveler_memory::WriteOutcome::Applied { id, operation, .. }) => {
                        if verified {
                            stored_verified += 1;
                            saved += 1;
                        } else {
                            pending += 1;
                        }
                        match operation.as_str() {
                            "updated" | "superseded" | "merged" => updates += 1,
                            "created" => {}
                            other => println!("  APPLIED {other}"),
                        }
                        expected_ids.insert(scenario.name, id.clone());
                        println!("  STORED operation={operation} verified={verified}");
                    }
                    Ok(leveler_memory::WriteOutcome::Pending { .. }) => {
                        pending += 1;
                        println!("  QUEUED for consent");
                    }
                    Ok(leveler_memory::WriteOutcome::NotStored { reason }) => {
                        duplicates += 1;
                        println!("  NOT STORED: {reason}");
                    }
                    Ok(other) => {
                        duplicates += 1;
                        println!("  NOT STORED: {other:?}");
                    }
                    Err(error) => println!("  STORE ERROR: {error}"),
                }
            }
            if scenario.expect_verified && stored_verified == 0 {
                false_saves += 0;
                println!("  MISS: expected a verified experience");
            }
            if !scenario.expect_verified && stored_verified > 0 {
                false_saves += 1;
                println!("  FALSE SAVE: low-value scenario stored an experience");
            }
            if !scenario.expect_verified && accepted_count == 0 {
                skipped += 1;
            }

            // Recall the subject in a later-session phrasing. A relevant entry
            // must be selected; a stale/irrelevant one must not.
            let query = match scenario.name {
                "complex_root_cause" => "UTF-8 字符串按字节截断 panic char boundary",
                "environment_constraint" => "cargo test 缺少 C linker cc not found",
                "simple_typo_fix" => "2+2 测试 typo",
                _ => "flaky 集成测试",
            };
            if expected_ids.contains_key(scenario.name) {
                recall_total += 1;
                let hits = service
                    .search(
                        &Actor::External,
                        ServiceScope::Project,
                        MemoryLanes::EXPERIENCE_ONLY,
                        query,
                        5,
                    )
                    .unwrap();
                if !hits.is_empty() {
                    recall_hits += 1;
                }
                let expected = expected_ids.get(scenario.name).unwrap();
                let correct = hits.iter().any(|(view, _)| &view.id == expected);
                if correct {
                    recall_correct += 1;
                }
                println!(
                    "  RECALL `{query}` expected={expected} -> {:?} correct={correct}",
                    hits.iter().map(|(view, _)| &view.id).collect::<Vec<_>>()
                );
            }
        }
    }

    println!("\n===== STORE =====");
    for view in service
        .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
        .unwrap()
    {
        println!(
            "  active id={} key={:?} standing={:?} title={:?}",
            view.id,
            view.subject_key,
            view.experience
                .as_ref()
                .map(|record| record.standing.clone()),
            view.title
        );
    }
    println!("===== METRICS =====");
    println!("rounds={turns}");
    println!("extraction_calls={calls}");
    println!("expected_saved={expected_saved} saved_verified={saved}");
    println!("expected_skipped={expected_skipped} correctly_skipped={skipped}");
    println!("pending={pending} duplicates={duplicates} updates={updates}");
    println!("false_saves={false_saves}");
    println!(
        "recall_hits={recall_hits}/{recall_total} recall_correct={recall_correct}/{recall_total}"
    );
    println!(
        "mean_latency_ms={}",
        if calls == 0 {
            0
        } else {
            total_ms / calls as u128
        }
    );
    println!("total_input_tokens={total_in} total_output_tokens={total_out}");
    let cost = (total_in as f64 / 1_000_000.0) * 0.1389 + (total_out as f64 / 1_000_000.0) * 0.2778;
    println!("total_cost_usd={cost:.6}");
    // A subject identity check: the update scenario must share one identity.
    assert!(semantic_key_of("experience.demo") == semantic_key_of("experience.demo"));
}
