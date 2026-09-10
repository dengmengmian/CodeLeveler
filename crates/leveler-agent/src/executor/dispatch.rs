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

/// The commands a tool reported as actually executed (`metadata.executed_commands`).
///
/// The execution layer states this; the executor never re-derives it from tool
/// names or arguments. A tool that runs nothing reports nothing.
/// The canonical unified diff an edit tool reported for what it actually
/// changed. Presentation truth, produced by the layer that owns the location:
/// no reader may re-derive a line number from the model's request instead.
pub(crate) fn extract_applied_diff(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get("applied_diff")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn extract_executed_commands(metadata: &serde_json::Value) -> Vec<Vec<String>> {
    let Some(commands) = metadata.get("executed_commands").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    commands
        .iter()
        .filter_map(|c| {
            let words: Vec<String> = c
                .as_array()?
                .iter()
                .filter_map(|w| w.as_str().map(String::from))
                .collect();
            (!words.is_empty()).then_some(words)
        })
        .collect()
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

/// Record mutation evidence for any tool call that modified files.
///
/// `record_paths` keeps its original meaning per call site (the first-touch
/// delta on the sequential path); `MutationRecord` gating semantics are
/// unchanged. What IS new: every mutating call — including a re-edit of a file
/// already in the modified set — bumps `total_mutation_ops` and persists the
/// ledger, because refinement used to be invisible here (R011-F1).
pub(crate) fn note_tool_side_effects(
    ledger: &mut EvidenceLedger,
    tool_call_id: &str,
    tool: &str,
    record_paths: Vec<String>,
    plan_state: &PlanState,
    observer: &mut (dyn FnMut(AgentEvent) + Send),
) {
    ledger.note_mutation_op();
    if !record_paths.is_empty() {
        ledger.record_mutation(tool_call_id, tool, record_paths);
    }
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

/// A tool call's arguments, bounded but still parseable.
///
/// Cutting the serialized text left invalid JSON, and the interface reads
/// these arguments to learn which file an edit touched — so the largest edits,
/// the ones a reader most wants named, arrived as an unparseable blob and lost
/// their filename. Bound the long string VALUES instead and re-serialize: the
/// envelope stays valid and a patch keeps its `*** Update File:` header.
pub(crate) fn compact_json(value: &serde_json::Value) -> String {
    let whole = value.to_string();
    if whole.chars().count() <= ARGUMENTS_MAX {
        return whole;
    }
    let mut bounded = value.clone();
    bound_strings(&mut bounded, ARGUMENTS_MAX);
    let out = bounded.to_string();
    // A single enormous key, or a shape with no strings to bound, still has to
    // be capped; that case keeps the old behaviour rather than growing.
    if out.chars().count() <= ARGUMENTS_MAX {
        out
    } else {
        preview(&out)
    }
}

/// Budget for one call's arguments. Matches [`preview`]'s cap so the two
/// bounds do not drift apart.
const ARGUMENTS_MAX: usize = 1200;

/// Shorten every string in `value` so the whole serializes within `budget`,
/// longest first. Each cut keeps its head — a patch's header lines — and marks
/// itself with an ellipsis.
fn bound_strings(value: &mut serde_json::Value, budget: usize) {
    // Leave room for the JSON scaffolding around the strings.
    let overhead = value.to_string().chars().count() - total_string_len(value);
    let allowance = budget.saturating_sub(overhead).max(1);
    let total = total_string_len(value);
    if total <= allowance {
        return;
    }
    let mut strings: Vec<&mut String> = Vec::new();
    collect_strings(value, &mut strings);
    // Give every string an equal share, then hand back what the short ones do
    // not use — one long patch beside a short path keeps almost all of it.
    let mut share = allowance / strings.len().max(1);
    let mut settled = 0usize;
    loop {
        let (small, large): (Vec<usize>, Vec<usize>) =
            (0..strings.len()).partition(|i| strings[*i].chars().count() <= share);
        if small.len() == settled || large.is_empty() {
            break;
        }
        settled = small.len();
        let used: usize = small.iter().map(|i| strings[*i].chars().count()).sum();
        share = allowance.saturating_sub(used) / large.len().max(1);
    }
    for s in strings {
        if s.chars().count() > share {
            let head: String = s.chars().take(share.saturating_sub(1).max(1)).collect();
            *s = format!("{head}…");
        }
    }
}

fn total_string_len(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.chars().count(),
        serde_json::Value::Array(a) => a.iter().map(total_string_len).sum(),
        serde_json::Value::Object(o) => o.values().map(total_string_len).sum(),
        _ => 0,
    }
}

fn collect_strings<'a>(value: &'a mut serde_json::Value, out: &mut Vec<&'a mut String>) {
    match value {
        serde_json::Value::String(s) => out.push(s),
        serde_json::Value::Array(a) => {
            for v in a {
                collect_strings(v, out);
            }
        }
        serde_json::Value::Object(o) => {
            for v in o.values_mut() {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Read-only tools permitted during plan explore rounds (before first plan).
///
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
    observer: &mut (dyn FnMut(AgentEvent) + Send),
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
        applied_diff: None,
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

    /// A second edit of a file already in the modified set arrives here with
    /// an EMPTY first-touch delta. It is still a change to the tree, and a
    /// test that passed before it is no longer proof about the tree as it
    /// stands. Freshness has to move on every mutating call, not only on the
    /// first touch of each path — or a refinement edit leaves a green run
    /// looking current when it is not.
    #[test]
    fn a_re_edit_of_an_already_modified_file_stales_prior_verification() {
        let mut ledger = EvidenceLedger::default();
        let plan = PlanState::default();
        ledger.record_mutation("c1", "apply_patch", vec!["a.rs".into()]);
        ledger.record_verify("v1", "go test ./...", 0);
        assert!(
            ledger.has_fresh_successful_verify(),
            "green right after the first edit"
        );
        note_tool_side_effects(&mut ledger, "c2", "apply_patch", vec![], &plan, &mut |_| {});
        assert!(
            !ledger.has_fresh_successful_verify(),
            "a re-edit must invalidate the earlier green run"
        );
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

#[cfg(test)]
mod compact_json_tests {
    use super::compact_json;

    /// A tool call's arguments are the only place the interface learns which
    /// file an edit touched. Truncating the SERIALIZED form leaves invalid
    /// JSON, so a large edit arrives unparseable and its row loses the
    /// filename — measured on a real 25-minute session, where 3 of 81 calls
    /// were cut this way and every one of them was an `apply_patch`.
    #[test]
    fn a_patch_too_large_to_carry_whole_still_arrives_as_json() {
        let body: String =
            std::iter::repeat_n("+// a line of a very large new file\n", 200).collect();
        let patch = format!(
            "*** Begin Patch\n*** Add File: pkg/yqlib/count_documents_test.go\n{body}*** End Patch"
        );
        let value = serde_json::json!({ "patch": patch });
        let compacted = compact_json(&value);

        let parsed: serde_json::Value = serde_json::from_str(&compacted)
            .expect("a truncated argument must still parse as JSON");
        let carried = parsed["patch"]
            .as_str()
            .expect("the patch survives as a string");
        assert!(
            carried.contains("*** Add File: pkg/yqlib/count_documents_test.go"),
            "the header names the file and must survive: {carried:.120}"
        );
        assert!(
            compacted.len() < value.to_string().len(),
            "a large patch is still bounded"
        );
    }

    #[test]
    fn arguments_that_fit_are_passed_through_untouched() {
        let value = serde_json::json!({ "path": "src/lib.rs", "max_depth": 3 });
        assert_eq!(compact_json(&value), value.to_string());
    }
}
