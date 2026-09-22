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

#[derive(Clone, Copy)]
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
    let db = app.open_database().await.ok()?;
    let raw = leveler_engine::RawTranscript::load_lossy(&db, session_id)
        .await
        .ok()?;
    let transcript_len = raw.stored_len();

    let context = if raw.messages.is_empty() {
        match kind {
            AssistKind::PromptSuggestion => repository_context(&app.layout.repo_root)?,
            AssistKind::AwaySummary => return None,
        }
    } else {
        if matches!(kind, AssistKind::AwaySummary)
            && raw
                .messages
                .iter()
                .filter(|message| message.role == Role::User && !message.text_content().is_empty())
                .count()
                < 2
        {
            return None;
        }
        transcript_context(&raw.messages)?
    };

    let (instruction, max_output_tokens, max_chars) = match kind {
        AssistKind::PromptSuggestion => (
            "Predict the single most likely next message the user would type. Use the user's language. Output only that message, with no quotes, label, explanation, or markdown. Base it only on the supplied context; do not invent completed work.",
            64,
            120,
        ),
        AssistKind::AwaySummary => (
            "Write one concise recap sentence for a user returning after being idle. In the user's language, state where the work reached, the current blocker or status when known, and the likely next step. Output only the sentence, with no label, bullets, markdown, or invented facts.",
            128,
            240,
        ),
    };
    let prompt = format!("{instruction}\n\nContext:\n{context}");
    let mut request = ModelRequest::new(model, vec![Message::text(Role::User, prompt)]);
    request.tool_choice = ToolChoice::None;
    request.max_output_tokens = Some(max_output_tokens);
    request.temperature = Some(0.2);

    let response =
        tokio::time::timeout(ASSIST_TIMEOUT, app.registry.generate(request, cancellation))
            .await
            .ok()?
            .ok()?;
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
