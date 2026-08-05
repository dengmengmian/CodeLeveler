use leveler_lifecycle::{EvidenceLedger, PlanState, PlanStep};
use leveler_model::{ContentPart, ImageSource, ToolCall, ToolResultContent};

use super::AgentEvent;

/// Pull the structured plan an update_plan call exposed via `metadata.plan`,
/// so the executor can surface it as [`AgentEvent::PlanUpdated`].
pub(crate) fn extract_plan(metadata: &serde_json::Value) -> Option<Vec<PlanStep>> {
    let steps: Vec<PlanStep> = serde_json::from_value(metadata.get("plan")?.clone()).ok()?;
    (!steps.is_empty()).then_some(steps)
}

/// Pull a base64 image a tool exposed via `metadata.image` into an image content
/// part, so the executor can show it to a vision model on the next request.
pub(crate) fn extract_image(metadata: &serde_json::Value) -> Option<ContentPart> {
    let img = metadata.get("image")?;
    let media_type = img.get("media_type")?.as_str()?.to_string();
    let data = img.get("data")?.as_str()?.to_string();
    Some(ContentPart::Image {
        source: ImageSource::Base64 { media_type, data },
    })
}

pub(crate) fn collect_modified(metadata: &serde_json::Value, out: &mut Vec<String>) {
    if let Some(files) = metadata.get("modified_files").and_then(|v| v.as_array()) {
        for f in files {
            if let Some(s) = f.as_str()
                && !out.iter().any(|e| e == s)
            {
                out.push(s.to_string());
            }
        }
    }
}

/// Paths present in `after` but not in `before` (this tool call's net writes).
pub(crate) fn newly_modified_paths(before: &[String], after: &[String]) -> Vec<String> {
    after
        .iter()
        .filter(|p| !before.iter().any(|e| e == *p))
        .cloned()
        .collect()
}

/// Record mutation evidence for any tool that produced new modified_files.
pub(crate) fn note_tool_side_effects(
    ledger: &mut EvidenceLedger,
    tool_call_id: &str,
    tool: &str,
    newly: Vec<String>,
    plan_state: &PlanState,
    observer: &mut dyn FnMut(AgentEvent),
) {
    if newly.is_empty() {
        return;
    }
    ledger.record_mutation(tool_call_id, tool, newly);
    ledger.plan = plan_state.clone();
    observer(AgentEvent::EvidenceLedgerUpdated {
        ledger: ledger.clone(),
    });
}

/// The text a user's request carries. Images and other parts hold no language.
pub(crate) fn text_of(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn compact_json(value: &serde_json::Value) -> String {
    preview(&value.to_string())
}

/// Read-only tools permitted during plan explore rounds (before first plan).
pub(crate) fn is_plan_explore_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "list_files"
            | "grep"
            | "find_files"
            | "find_symbol"
            | "read_symbol"
            | "find_references"
            | "web_search"
            | "web_fetch"
            | "load_skill"
    )
}

/// Keep one-step requests lightweight, but require a machine-readable plan for
/// requests that already spell out several independently checkable pieces of
/// work. The gate intentionally uses only obvious structure; uncertain tasks
/// may still create a plan voluntarily without blocking simple edits.
pub(crate) fn task_needs_structured_plan(task: &str) -> bool {
    fn is_output_labelled_url(line: &str) -> bool {
        let line = line.trim_start();
        let Some(payload) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("• "))
        else {
            return false;
        };
        let Some((label, value)) = payload.split_once(':') else {
            return false;
        };
        let value = value.trim_start();
        !label.is_empty()
            && label
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ' '))
            && (value.starts_with("http://") || value.starts_with("https://"))
    }

    fn is_bullet_item(line: &str) -> bool {
        let line = line.trim_start();
        (line.starts_with("- ") || line.starts_with("* ") || line.starts_with("• "))
            && !is_output_labelled_url(line)
    }

    fn is_numbered_item(line: &str) -> bool {
        let line = line.trim_start();
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        digits > 0
            && matches!(
                line.as_bytes().get(digits).copied(),
                Some(b'.' | b')' | b':')
            )
    }

    let numbered_items = task
        .lines()
        .filter(|line| is_numbered_item(line))
        .take(2)
        .count();
    if numbered_items >= 2 {
        return true;
    }

    let bullet_items = task
        .lines()
        .filter(|line| is_bullet_item(line))
        .take(3)
        .count();
    if bullet_items >= 3 {
        return true;
    }

    let sentence_count = task
        .split(['。', '！', '？', '.', '!', '?', '\n'])
        .filter(|part| !part.trim().is_empty())
        .take(4)
        .count();
    let normalized = task.to_lowercase();
    let concern_markers = [
        "而且",
        "还要",
        "然后",
        "最后",
        "并且",
        "同时",
        "also",
        "and also",
        "finally",
        "additionally",
    ]
    .into_iter()
    .filter(|marker| normalized.contains(marker))
    .take(2)
    .count();
    task.chars().count() >= 60 && sentence_count >= 3 && concern_markers >= 2
}

/// Refuse a call a guard stopped before it ran, and feed the reason back to the
/// model as the call's result.
///
/// The call is announced first even though it never executes: a `ToolResult`
/// whose id no `ToolCall` ever introduced reaches the UI as an id it has never
/// seen, leaving it with no name or arguments to render — the row comes out
/// blank. A refusal has to say what was refused.
pub(crate) fn deny_call(
    observer: &mut dyn FnMut(AgentEvent),
    call: ToolCall,
    message: String,
) -> ContentPart {
    observer(AgentEvent::ToolCall {
        id: call.id.as_str().to_string(),
        name: call.name.clone(),
        arguments: compact_json(&call.arguments),
        parallel: false,
    });
    observer(AgentEvent::ToolResult {
        id: call.id.as_str().to_string(),
        name: call.name.clone(),
        is_error: true,
        preview: message.clone(),
    });
    ContentPart::ToolResult {
        result: ToolResultContent {
            call_id: call.id,
            content: message,
            is_error: true,
        },
    }
}

pub(crate) fn preview(s: &str) -> String {
    const MAX: usize = 1200;
    // Drop ANSI before truncating so color codes neither pollute the TUI nor
    // burn the preview budget.
    let clean = leveler_core::sanitize_terminal_output(s);
    if clean.chars().count() <= MAX {
        clean
    } else {
        let truncated: String = clean.chars().take(MAX).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod mutation_ledger_tests {
    use super::*;

    #[test]
    fn newly_modified_paths_only_returns_delta() {
        let before = vec!["a.rs".into(), "b.rs".into()];
        let after = vec!["a.rs".into(), "b.rs".into(), "c.rs".into()];
        assert_eq!(
            newly_modified_paths(&before, &after),
            vec!["c.rs".to_string()]
        );
        assert!(newly_modified_paths(&after, &after).is_empty());
    }

    #[test]
    fn note_tool_side_effects_records_run_command_mutations() {
        let mut ledger = EvidenceLedger::default();
        let plan = PlanState::default();
        let mut events = 0u32;
        note_tool_side_effects(
            &mut ledger,
            "c1",
            "run_command",
            vec!["generated.rs".into()],
            &plan,
            &mut |_| {
                events += 1;
            },
        );
        assert_eq!(ledger.mutations.len(), 1);
        assert_eq!(ledger.mutations[0].tool, "run_command");
        assert_eq!(ledger.mutations[0].paths, vec!["generated.rs".to_string()]);
        assert_eq!(events, 1);
    }
}
