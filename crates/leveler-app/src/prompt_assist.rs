//! Auxiliary, non-authoritative model text for idle UI affordances.
//!
//! Prompt suggestions and away summaries share bounded context loading and a
//! short, tool-free model request. They never enter the conversation, task
//! lifecycle, verification, or completion evidence.

use std::time::Duration;

use leveler_core::SessionId;
use leveler_model::{Message, ModelRef, ModelRequest, ModelRuntime, Role, ToolChoice};
use tokio_util::sync::CancellationToken;

use crate::Application;

const ASSIST_TIMEOUT: Duration = Duration::from_secs(12);
const CONTEXT_CHAR_LIMIT: usize = 8_000;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AssistKind {
    PromptSuggestion,
    AwaySummary,
}

pub(crate) struct GeneratedAssist {
    pub text: String,
    pub transcript_len: u64,
}

pub(crate) async fn generate(
    app: &Application,
    session_id: &SessionId,
    model: ModelRef,
    kind: AssistKind,
    cancellation: CancellationToken,
) -> Option<GeneratedAssist> {
    // A switched-off affordance makes no model call at all: the user's
    // `[assist]` setting is the authority, not the UI's idle timer.
    if !app.assist_enabled(kind) {
        return None;
    }
    let db = app.open_database().await.ok()?;
    let raw = leveler_engine::RawTranscript::load_lossy(&db, session_id)
        .await
        .ok()?;
    let transcript_len = raw.stored_len();

    let context = if raw.messages.is_empty() {
        match kind {
            AssistKind::PromptSuggestion => repository_context(app.layout.primary_workspace()?)?,
            AssistKind::AwaySummary => return None,
        }
    } else {
        transcript_context(&raw.messages)?
    };

    let style = app
        .registry
        .profile(&model)
        .await
        .map(|profile| profile.reasoning.style)
        .unwrap_or_default();
    let request = assist_request(model.clone(), kind, &context, style);
    let max_chars = assist_char_limit(kind);

    let started = std::time::Instant::now();
    let outcome =
        tokio::time::timeout(ASSIST_TIMEOUT, app.registry.generate(request, cancellation))
            .await
            .map_err(|_| {
                leveler_model::ModelError::new(
                    leveler_model::ModelErrorKind::Timeout,
                    "assist timed out",
                )
            })
            .and_then(|result| result);
    let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let kind = match kind {
        AssistKind::PromptSuggestion => leveler_storage::ModelCallKind::PromptSuggestion,
        AssistKind::AwaySummary => leveler_storage::ModelCallKind::AwaySummary,
    };
    crate::observability::record_auxiliary_call(
        app,
        &db,
        session_id,
        kind,
        &model.provider,
        &model.model,
        Some("low"),
        &outcome,
        latency_ms,
        None,
        None,
    )
    .await
    .ok()?;
    let response = outcome.ok()?;
    let text = normalize_prediction(&response.message.text_content(), max_chars)?;
    Some(GeneratedAssist {
        text,
        transcript_len,
    })
}

pub(crate) async fn transcript_len(app: &Application, session_id: &SessionId) -> Option<u64> {
    let db = app.open_database().await.ok()?;
    leveler_engine::RawTranscript::load_lossy(&db, session_id)
        .await
        .ok()
        .map(|raw| raw.stored_len())
}

fn assist_char_limit(kind: AssistKind) -> usize {
    match kind {
        AssistKind::PromptSuggestion => 120,
        AssistKind::AwaySummary => 240,
    }
}

/// One short UI sentence. Thinking stays off when the route can say so:
/// a 128-token cap spent entirely on reasoning cuts the sentence mid-way
/// (`finish_reason = length`). Styles that cannot turn thinking off keep it,
/// and get a budget large enough for a short think plus the sentence.
fn assist_request(
    model: ModelRef,
    kind: AssistKind,
    context: &str,
    style: leveler_model::ReasoningStyle,
) -> ModelRequest {
    let (instruction, max_output_tokens) = match kind {
        AssistKind::PromptSuggestion => (
            "Predict the single most likely next message the user would type. Use the user's language. Output only that message, with no quotes, label, explanation, or markdown. Base it only on the supplied context; do not invent completed work.",
            256,
        ),
        AssistKind::AwaySummary => (
            "Write one concise recap sentence of where this conversation reached. In the user's language, state the outcome, the current blocker or status when known, and the likely next step. Output only the sentence, with no label, bullets, markdown, or invented facts.",
            1024,
        ),
    };
    let mut request = ModelRequest::new(
        model,
        vec![Message::text(
            Role::User,
            format!("{instruction}\n\nContext:\n{context}"),
        )],
    );
    request.tool_choice = ToolChoice::None;
    request.max_output_tokens = Some(max_output_tokens);
    request.temperature = Some(0.2);
    request.reasoning_effort = Some(leveler_model::ReasoningEffort::Low);
    // Only the thinking-flag style has a word for "off". The effort styles
    // (Kimi K3/K3.1, K2.8) are told how much to think, not whether to: asking
    // them to disable would drop the effort with it and silently run the
    // provider's own default, so they keep the explicit `low` above instead.
    request.thinking_disabled = matches!(style, leveler_model::ReasoningStyle::ThinkingFlag);
    request
}

fn repository_context(repo: &std::path::Path) -> Option<String> {
    let branch = leveler_core::git_stdout(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let commits =
        leveler_core::git_stdout(repo, &["log", "-5", "--pretty=format:%s"]).unwrap_or_default();
    let status = leveler_core::git_stdout(repo, &["status", "--short"]).unwrap_or_default();
    Some(format!(
        "Repository branch: {}\nRecent commits:\n{}\nWorking tree:\n{}",
        branch.trim(),
        bounded(&commits, 2_000),
        bounded(&status, 2_000)
    ))
}

fn transcript_context(messages: &[Message]) -> Option<String> {
    let mut rows = messages
        .iter()
        .rev()
        .filter_map(|message| {
            let role = match message.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::System | Role::Tool => return None,
            };
            let text = message.text_content();
            (!text.trim().is_empty()).then(|| format!("{role}: {}", text.trim()))
        })
        .take(12)
        .collect::<Vec<_>>();
    rows.reverse();
    let context = bounded_tail(&rows.join("\n---\n"), CONTEXT_CHAR_LIMIT);
    (!context.is_empty()).then_some(context)
}

fn bounded(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn bounded_tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(max_chars)).collect()
}

fn normalize_prediction(text: &str, max_chars: usize) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?.trim();
    let line = line
        .trim_start_matches(['-', '•'])
        .trim()
        .strip_prefix("建议：")
        .or_else(|| line.strip_prefix("建议:"))
        .or_else(|| line.strip_prefix("Suggestion:"))
        .or_else(|| line.strip_prefix("suggestion:"))
        .or_else(|| line.strip_prefix("recap:"))
        .unwrap_or(line)
        .trim()
        .trim_matches(|c| matches!(c, '`' | '"' | '\'' | '“' | '”'))
        .trim();
    let lower = line.to_ascii_lowercase();
    if line.is_empty()
        || matches!(lower.as_str(), "none" | "n/a")
        || line.contains("没有合适的建议")
        || line.contains("无法生成")
    {
        return None;
    }
    let chars = line.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return Some(line.to_string());
    }
    Some(format!(
        "{}…",
        chars[..max_chars].iter().collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prediction_is_one_clean_line_without_model_wrapping() {
        assert_eq!(
            normalize_prediction("  建议：`检查配置覆盖顺序`\n额外解释", 80).as_deref(),
            Some("检查配置覆盖顺序")
        );
    }

    #[test]
    fn blank_or_meta_prediction_is_rejected() {
        assert_eq!(normalize_prediction("   \n", 80), None);
        assert_eq!(normalize_prediction("没有合适的建议", 80), None);
    }

    #[test]
    fn a_thinking_model_does_not_spend_the_recap_budget_on_reasoning() {
        let request = assist_request(
            ModelRef::parse("deepseek/deepseek-flash").expect("model ref"),
            AssistKind::AwaySummary,
            "已确认这是本地编程智能体",
            leveler_model::ReasoningStyle::ThinkingFlag,
        );
        assert!(request.thinking_disabled);
        assert!(request.max_output_tokens.unwrap() > 128);
    }

    #[test]
    fn a_route_that_cannot_disable_thinking_is_left_able_to_encode() {
        let request = assist_request(
            ModelRef::parse("anthropic/claude").expect("model ref"),
            AssistKind::AwaySummary,
            "context",
            leveler_model::ReasoningStyle::AdaptiveThinking,
        );
        assert!(!request.thinking_disabled);
    }

    /// A route whose thinking is addressed by `reasoning_effort` (Kimi K3/K3.1,
    /// K2.8) has no word for "off". The assist request must keep its explicit
    /// low effort instead: asking to disable thinking there drops the effort
    /// too, so the encoder would send neither field and the provider would run
    /// its own default — the maximum — on a budget sized for a short thought.
    #[test]
    fn an_effort_route_keeps_its_low_effort_instead_of_asking_for_off() {
        for style in [
            leveler_model::ReasoningStyle::OpenAiEffort,
            leveler_model::ReasoningStyle::None,
        ] {
            let request = assist_request(
                ModelRef::parse("kimi/k3.1-pro").expect("model ref"),
                AssistKind::PromptSuggestion,
                "context",
                style,
            );
            assert!(
                !request.thinking_disabled,
                "{style:?}: this style has no `off` word, so it must keep its effort"
            );
            assert_eq!(
                request.reasoning_effort,
                Some(leveler_model::ReasoningEffort::Low),
                "{style:?}"
            );
        }
    }

    #[test]
    fn prediction_is_bounded_on_character_boundaries() {
        let text = "修".repeat(100);
        let out = normalize_prediction(&text, 12).expect("prediction");
        assert_eq!(out.chars().count(), 13);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn transcript_budget_keeps_the_most_recent_message() {
        let messages = vec![
            Message::text(Role::User, "a".repeat(CONTEXT_CHAR_LIMIT + 100)),
            Message::text(Role::Assistant, "LATEST"),
        ];
        let context = transcript_context(&messages).expect("context");
        assert!(context.contains("LATEST"));
        assert!(context.chars().count() <= CONTEXT_CHAR_LIMIT);
    }
}
