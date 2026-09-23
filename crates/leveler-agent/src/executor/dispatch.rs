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

/// The process facts a command call reported: its exit code, and — when it
/// was cancelled — whether its process tree was confirmed gone.
pub(crate) fn extract_command_facts(
    metadata: &serde_json::Value,
) -> (Option<i32>, Option<leveler_execution::CommandStop>) {
    let exit_code = metadata
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    let stop = metadata
        .get("stop")
        .and_then(|stop| serde_json::from_value(stop.clone()).ok());
    (exit_code, stop)
}

/// Live command output cut into whole lines before it leaves the loop, so a
/// client never sees half a line and a secret is never split across two
/// sanitizer passes. What is left at the end is flushed as-is.
#[derive(Default)]
pub(crate) struct OutputLines {
    stdout: String,
    stderr: String,
    /// The last lines that left, for a result that has no output of its own:
    /// a stopped command returns only "cancelled".
    tail: std::collections::VecDeque<String>,
}

/// How much of a stopped command's output its result repeats.
const STOPPED_TAIL_LINES: usize = 40;
const STOPPED_TAIL_BYTES: usize = 4096;

impl OutputLines {
    /// Absorb one chunk; returns the complete lines it finished, sanitized.
    pub(crate) fn push(
        &mut self,
        chunk: leveler_execution::OutputChunk,
    ) -> Option<(leveler_execution::OutputStream, String)> {
        let buffer = self.buffer(chunk.stream);
        buffer.push_str(&chunk.text);
        let cut = buffer.rfind('\n')? + 1;
        let complete: String = buffer.drain(..cut).collect();
        let text = sanitize_output(&complete);
        self.remember(&text);
        Some((chunk.stream, text))
    }

    /// Whatever partial lines remain once the command has ended.
    pub(crate) fn flush(&mut self) -> Vec<(leveler_execution::OutputStream, String)> {
        use leveler_execution::OutputStream::{Stderr, Stdout};
        [
            (Stdout, std::mem::take(&mut self.stdout)),
            (Stderr, std::mem::take(&mut self.stderr)),
        ]
        .into_iter()
        .filter(|(_, rest)| !rest.is_empty())
        .map(|(stream, rest)| (stream, sanitize_output(&rest)))
        .inspect(|(_, text)| self.remember(text))
        .collect()
    }

    /// The output that already left, bounded, for a stopped command's result.
    pub(crate) fn stopped_tail(&self) -> Option<String> {
        if self.tail.is_empty() {
            return None;
        }
        let mut lines: Vec<&str> = Vec::new();
        let mut bytes = 0;
        for line in self.tail.iter().rev() {
            if bytes + line.len() > STOPPED_TAIL_BYTES {
                break;
            }
            bytes += line.len() + 1;
            lines.push(line);
        }
        lines.reverse();
        Some(lines.join("\n"))
    }

    fn remember(&mut self, text: &str) {
        for line in text.lines() {
            if self.tail.len() == STOPPED_TAIL_LINES {
                self.tail.pop_front();
            }
            self.tail.push_back(line.to_string());
        }
    }

    fn buffer(&mut self, stream: leveler_execution::OutputStream) -> &mut String {
        match stream {
            leveler_execution::OutputStream::Stdout => &mut self.stdout,
            leveler_execution::OutputStream::Stderr => &mut self.stderr,
        }
    }
}

/// Output leaving the loop gets the same treatment the finished result does:
/// terminal control sequences stripped and concrete secret values replaced.
fn sanitize_output(text: &str) -> String {
    let clean = leveler_core::sanitize_terminal_output(text);
    leveler_core::sanitize_model_visible(&clean).0
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
    // The ledger mirrors the drive's live plan. A drive whose in-memory plan
    // is still empty must not erase a plan the turn was seeded with: the plan
    // is only ever replaced by a real declaration, never by "nothing yet".
    if !plan_state.is_empty() {
        ledger.plan = plan_state.clone();
    }
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
        exit_code: None,
        stop: None,
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
    /// an EMPTY first-touch delta. It is still a change to the tree and must
    /// advance the mutation operation counter.
    #[test]
    fn a_re_edit_of_an_already_modified_file_counts_as_a_mutation() {
        let mut ledger = EvidenceLedger::default();
        let plan = PlanState::default();
        ledger.record_mutation("c1", "apply_patch", vec!["a.rs".into()]);
        let before = ledger.total_mutation_ops;
        note_tool_side_effects(&mut ledger, "c2", "apply_patch", vec![], &plan, &mut |_| {});
        assert_eq!(ledger.total_mutation_ops, before + 1);
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

    /// The evidence ledger mirrors the drive's live plan. A resume seeds both,
    /// but a mutating call whose in-memory plan has not been populated yet must
    /// not bleed the seeded plan empty — that would silently discard the
    /// interruption's progress. A real declaration still replaces it.
    #[test]
    fn an_empty_live_plan_does_not_erase_a_seeded_ledger_plan() {
        let seeded = PlanState {
            steps: vec![leveler_lifecycle::PlanStep {
                step: "implement core".into(),
                status: "in_progress".into(),
                id: Some("s1".into()),
                origin: leveler_lifecycle::PlanOrigin::ModelExplicit,
            }],
        };
        let mut ledger = EvidenceLedger {
            plan: seeded.clone(),
            ..Default::default()
        };
        note_tool_side_effects(
            &mut ledger,
            "c1",
            "apply_patch",
            vec!["a.rs".into()],
            &PlanState::default(),
            &mut |_| {},
        );
        assert_eq!(
            ledger.plan, seeded,
            "a seeded plan must survive an empty live plan"
        );

        let declared = PlanState {
            steps: vec![leveler_lifecycle::PlanStep {
                step: "done".into(),
                status: "completed".into(),
                id: None,
                origin: leveler_lifecycle::PlanOrigin::ModelExplicit,
            }],
        };
        note_tool_side_effects(
            &mut ledger,
            "c2",
            "apply_patch",
            vec![],
            &declared,
            &mut |_| {},
        );
        assert_eq!(
            ledger.plan, declared,
            "a real declaration still replaces the mirrored plan"
        );
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

#[cfg(test)]
mod output_lines_tests {
    use super::OutputLines;
    use leveler_execution::{OutputChunk, OutputStream};

    fn chunk(stream: OutputStream, text: &str) -> OutputChunk {
        OutputChunk {
            stream,
            text: text.to_string(),
        }
    }

    #[test]
    fn a_partial_line_waits_for_its_newline_per_stream() {
        let mut lines = OutputLines::default();
        assert_eq!(lines.push(chunk(OutputStream::Stdout, "Check")), None);
        assert_eq!(
            lines.push(chunk(OutputStream::Stderr, "warn\n")),
            Some((OutputStream::Stderr, "warn\n".to_string()))
        );
        assert_eq!(
            lines.push(chunk(OutputStream::Stdout, "ing a\nChecking b")),
            Some((OutputStream::Stdout, "Checking a\n".to_string()))
        );
        assert_eq!(
            lines.flush(),
            vec![(OutputStream::Stdout, "Checking b".to_string())]
        );
        assert!(lines.flush().is_empty());
    }

    /// A stopped command's result repeats only the latest output, bounded.
    #[test]
    fn the_stopped_tail_keeps_the_last_lines_only() {
        let mut lines = OutputLines::default();
        assert_eq!(lines.stopped_tail(), None);
        for i in 1..=100 {
            lines.push(chunk(OutputStream::Stdout, &format!("tick {i}\n")));
        }
        lines.push(chunk(OutputStream::Stdout, "partial"));
        lines.flush();
        let tail = lines.stopped_tail().expect("output was printed");
        assert!(tail.ends_with("tick 100\npartial"), "{tail}");
        assert!(!tail.contains("tick 60\n"), "{tail}");
        assert_eq!(tail.lines().count(), 40);
    }

    #[test]
    fn terminal_control_sequences_never_leave_the_loop() {
        let mut lines = OutputLines::default();
        let (_, text) = lines
            .push(chunk(OutputStream::Stdout, "\u{1b}[31mred\u{1b}[0m\n"))
            .expect("a whole line");
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(text.contains("red"));
    }
}
