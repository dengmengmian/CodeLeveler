//! Read-only projection of a tool call's full recorded result.
//!
//! The authority is the session transcript: the exact text the model received
//! is persisted there. When that text was bounded, the tool layer saved the
//! complete output to the content-addressed artifact store and the result
//! itself names the saved file. This module never trusts a client-supplied
//! path: references are parsed out of the stored result of THIS call, and only
//! files inside the runtime's own artifact store are served.

use std::path::PathBuf;

use leveler_client_protocol::{
    TOOL_OUTPUT_PAGE_MAX_BYTES, UiToolOutput, UiToolOutputPart, UiToolOutputReference,
    UiToolOutputState, UiToolOutputUnavailable,
};
use leveler_core::SessionId;
use leveler_execution::{ArtifactReadError, ArtifactStore};
use leveler_model::{ContentPart, Message};

use crate::AppError;

/// One complete artifact a recorded result references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputReference {
    pub path: PathBuf,
    pub label: &'static str,
    /// The hash the runtime wrote into a saved-reference line. When present,
    /// the file name must be exactly `<hash>.txt`; a forged path cannot borrow
    /// another artifact's identity.
    pub sha256: Option<String>,
    /// The byte size the runtime recorded. A mismatch means the file is not
    /// the saved output and is reported unavailable, not served as if it were.
    pub declared_bytes: Option<u64>,
}

/// The stored tool result of one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredToolResult {
    pub content: String,
}

/// Find the newest stored result for `call_id` in the session transcript.
///
/// A result written before a `/compact` or checkpoint cut is not in the
/// transcript any more; returning `None` says exactly that, never "empty
/// output".
pub(crate) async fn stored_result(
    db: &leveler_storage::Database,
    session_id: &SessionId,
    call_id: &str,
) -> Result<Option<StoredToolResult>, AppError> {
    let payloads = leveler_storage::MessageRepository::new(db)
        .load(session_id)
        .await
        .map_err(AppError::from)?;
    for payload in payloads.iter().rev() {
        let Ok(message) = serde_json::from_str::<Message>(payload) else {
            continue;
        };
        if message.role != leveler_model::Role::Tool {
            continue;
        }
        for part in &message.content {
            let ContentPart::ToolResult { result } = part else {
                continue;
            };
            if result.call_id.as_str() == call_id {
                return Ok(Some(StoredToolResult {
                    content: result.content.clone(),
                }));
            }
        }
    }
    Ok(None)
}

/// Project one page of a stored result, or the typed reason it cannot be
/// served. `index = None` is the model-visible result; `Some(i)` is the i-th
/// complete artifact the result references.
pub(crate) fn project(
    call_id: &str,
    stored: Option<&StoredToolResult>,
    index: Option<u32>,
    offset: u64,
    max_bytes: u32,
    store: &ArtifactStore,
) -> UiToolOutput {
    let Some(stored) = stored else {
        return unavailable(call_id, index, UiToolOutputUnavailable::NotSaved);
    };
    let references = extract_references(&stored.content);
    let shaped: Vec<UiToolOutputReference> = references
        .iter()
        .enumerate()
        .map(|(position, reference)| {
            let size = store.size_of(&reference.path).unwrap_or(0);
            let matches_identity = reference.sha256.as_deref().is_none_or(|hash| {
                reference
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == format!("{hash}.txt"))
            }) && reference
                .declared_bytes
                .is_none_or(|declared| declared == size);
            UiToolOutputReference {
                index: position as u32,
                label: reference.label.to_string(),
                size_bytes: size,
                available: matches_identity && store.resolve(&reference.path).is_ok(),
            }
        })
        .collect();
    let model_result_reduced = result_is_reduced(&stored.content);
    let full_output_lost = result_is_lost(&stored.content, !references.is_empty());
    let budget = (max_bytes as usize).clamp(1, TOOL_OUTPUT_PAGE_MAX_BYTES as usize);

    let Some(index) = index else {
        let page = page_str(&stored.content, offset, budget);
        return UiToolOutput {
            call_id: call_id.to_string(),
            state: UiToolOutputState::Available,
            unavailable: None,
            part: UiToolOutputPart::Result,
            label: "模型所见结果".to_string(),
            text: page.text,
            total_bytes: page.total,
            offset: page.offset,
            next_offset: page.next_offset,
            has_more: page.has_more,
            model_result_reduced,
            full_output_lost,
            references: shaped,
        };
    };

    let Some(reference) = references.get(index as usize) else {
        return unavailable_with_references(
            call_id,
            UiToolOutputUnavailable::UnknownPart,
            model_result_reduced,
            full_output_lost,
            shaped,
        );
    };
    let identity_ok = reference.sha256.as_deref().is_none_or(|hash| {
        reference
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == format!("{hash}.txt"))
    }) && reference.declared_bytes.is_none_or(|declared| {
        store
            .size_of(&reference.path)
            .is_ok_and(|size| size == declared)
    });
    if !identity_ok {
        return unavailable_with_references(
            call_id,
            UiToolOutputUnavailable::ArtifactMissing,
            model_result_reduced,
            full_output_lost,
            shaped,
        );
    }
    match store.read_page(&reference.path, offset, budget) {
        Ok(page) => UiToolOutput {
            call_id: call_id.to_string(),
            state: UiToolOutputState::Available,
            unavailable: None,
            part: UiToolOutputPart::Artifact { index },
            label: reference.label.to_string(),
            text: page.text,
            total_bytes: page.total_bytes,
            offset: page.offset,
            next_offset: page.next_offset,
            has_more: page.has_more,
            model_result_reduced,
            full_output_lost: false,
            references: shaped,
        },
        Err(ArtifactReadError::Missing | ArtifactReadError::NotArtifact) => {
            unavailable_with_references(
                call_id,
                UiToolOutputUnavailable::ArtifactMissing,
                model_result_reduced,
                full_output_lost,
                shaped,
            )
        }
        Err(ArtifactReadError::OffsetOutOfRange { .. } | ArtifactReadError::Io(_)) => {
            unavailable_with_references(
                call_id,
                UiToolOutputUnavailable::ReadFailed,
                model_result_reduced,
                full_output_lost,
                shaped,
            )
        }
    }
}

fn unavailable(call_id: &str, index: Option<u32>, reason: UiToolOutputUnavailable) -> UiToolOutput {
    let part = match index {
        Some(index) => UiToolOutputPart::Artifact { index },
        None => UiToolOutputPart::Result,
    };
    UiToolOutput {
        call_id: call_id.to_string(),
        state: UiToolOutputState::Unavailable,
        unavailable: Some(reason),
        part,
        label: part_label(&part),
        text: String::new(),
        total_bytes: 0,
        offset: 0,
        next_offset: 0,
        has_more: false,
        model_result_reduced: false,
        full_output_lost: false,
        references: Vec::new(),
    }
}

fn unavailable_with_references(
    call_id: &str,
    reason: UiToolOutputUnavailable,
    model_result_reduced: bool,
    full_output_lost: bool,
    references: Vec<UiToolOutputReference>,
) -> UiToolOutput {
    UiToolOutput {
        call_id: call_id.to_string(),
        state: UiToolOutputState::Unavailable,
        unavailable: Some(reason),
        part: UiToolOutputPart::Result,
        label: String::new(),
        text: String::new(),
        total_bytes: 0,
        offset: 0,
        next_offset: 0,
        has_more: false,
        model_result_reduced,
        full_output_lost,
        references,
    }
}

fn part_label(part: &UiToolOutputPart) -> String {
    match part {
        UiToolOutputPart::Result => "模型所见结果".to_string(),
        UiToolOutputPart::Artifact { .. } => "完整输出".to_string(),
    }
}

/// Complete artifacts the recorded result names.
///
/// Only two runtime-authored shapes are recognized: the registry's saved
/// reference, which is always the FIRST line, and the command layer's stream
/// locators, which are appended at the END (at most one per stream). A tool
/// result that merely quotes such text cannot move the reference away from
/// those positions.
pub(crate) fn extract_references(content: &str) -> Vec<OutputReference> {
    let mut out = Vec::new();
    let mut push = |reference: OutputReference| {
        if !reference.path.is_absolute()
            || out
                .iter()
                .any(|item: &OutputReference| item.path == reference.path)
        {
            return;
        }
        out.push(reference);
    };
    if let Some(line) = content.lines().next()
        && let Some(rest) = line.strip_prefix(leveler_model::tool_output::TOOL_OUTPUT_SAVED_PREFIX)
        && let Some((path, remainder)) = rest.trim_start().split_once(" | sha256:")
    {
        let (sha256, remainder) = remainder
            .split_once(" | ")
            .map(|(hash, rest)| (hash.trim(), rest))
            .unwrap_or((remainder.trim(), ""));
        let declared_bytes = remainder
            .split_once(" bytes.")
            .and_then(|(count, _)| count.trim().parse::<u64>().ok());
        push(OutputReference {
            path: PathBuf::from(path.trim()),
            label: "完整结果",
            sha256: (!sha256.is_empty()).then(|| sha256.to_string()),
            declared_bytes,
        });
    }
    // The command layer writes a locator as its own line, at the absolute end.
    // Bound the scan to the tail so a result that quotes a locator in its body
    // is not treated as if the command layer had emitted it.
    let tail: Vec<&str> = content
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(4)
        .collect();
    let mut locator_count = 0usize;
    for line in tail.into_iter().rev() {
        if locator_count >= 4 {
            break;
        }
        if line.len() > 512 || !line.ends_with(']') {
            continue;
        }
        for (prefix, label) in [
            ("[stdout full output: ", "stdout"),
            ("[stderr full output: ", "stderr"),
        ] {
            if let Some(path) = line
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(']'))
            {
                push(OutputReference {
                    path: PathBuf::from(path.trim()),
                    label,
                    sha256: None,
                    declared_bytes: None,
                });
                locator_count += 1;
            }
        }
    }
    out
}

/// Whether the recorded result says it is not the complete text.
pub(crate) fn result_is_reduced(content: &str) -> bool {
    if content.starts_with(leveler_model::tool_output::TOOL_OUTPUT_SAVED_PREFIX) {
        return true;
    }
    if content
        .lines()
        .any(leveler_model::tool_output::has_elision_marker)
    {
        return true;
    }
    !extract_references(content).is_empty()
}

/// Whether the result was cut with no saved copy: the bytes are gone.
fn result_is_lost(content: &str, has_reference: bool) -> bool {
    !has_reference
        && (content.starts_with(leveler_model::tool_output::TOOL_OUTPUT_SAVED_PREFIX)
            || leveler_model::tool_output::has_elision_marker(content))
}

struct Page {
    text: String,
    offset: u64,
    next_offset: u64,
    total: u64,
    has_more: bool,
}

/// Page a `String` without splitting a character. Offsets are byte offsets
/// into the stored text; a mid-character offset rounds DOWN, so a client that
/// repeats a byte range never loses a character.
fn page_str(text: &str, offset: u64, max_bytes: usize) -> Page {
    let total = text.len() as u64;
    let offset = offset.min(total);
    let start = leveler_core::floor_char_boundary(text, offset as usize);
    let mut end = leveler_core::floor_char_boundary(text, (start + max_bytes).min(text.len()));
    if end == start && start < text.len() {
        end = leveler_core::ceil_char_boundary(text, start + 1);
    }
    Page {
        text: text[start..end].to_string(),
        offset: start as u64,
        next_offset: end as u64,
        total,
        has_more: end < text.len(),
    }
}

/// The runtime's artifact store for this application's state directory.
pub(crate) fn artifact_store(app: &crate::Application) -> ArtifactStore {
    ArtifactStore::new(app.layout.state_dir.join("artifacts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_is_lossless_across_chinese_and_page_edges() {
        let text = "第一行\n第二行\n".repeat(20);
        let mut offset = 0u64;
        let mut joined = String::new();
        loop {
            let page = page_str(&text, offset, 7);
            joined.push_str(&page.text);
            if !page.has_more {
                assert_eq!(page.next_offset, text.len() as u64);
                break;
            }
            assert!(page.next_offset > offset);
            offset = page.next_offset;
        }
        assert_eq!(joined, text);
    }

    #[test]
    fn a_mid_character_offset_rounds_down_instead_of_losing_bytes() {
        let text = "中文中文";
        let page = page_str(text, 4, 3);
        assert_eq!(page.offset, 3, "offset 4 is inside the first '文'");
        assert!(page.text.starts_with('文'));
    }

    #[test]
    fn a_saved_reference_is_the_first_line_and_stream_locators_are_at_the_end() {
        let body = format!(
            "{}\nHEAD…TAIL",
            leveler_model::tool_output::saved_reference(
                "/state/artifacts/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.txt",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                120_000
            )
        );
        let refs = extract_references(&body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].label, "完整结果");
        assert!(refs[0].path.to_string_lossy().ends_with(".txt"));

        let command = "out…\n[stdout full output: /state/artifacts/aa.txt]\n[stderr full output: /state/artifacts/bb.txt]\n";
        let refs = extract_references(command);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].label, "stdout");
        assert_eq!(refs[1].label, "stderr");
    }

    #[test]
    fn quoted_locator_text_in_the_body_is_not_a_reference() {
        // A locator shape more than a few lines from the end cannot be the
        // command layer's own trailing pointer; it is body text.
        let body = format!(
            "log\n[stdout full output: /tmp/secret]\n{}",
            "x\n".repeat(10)
        );
        assert!(extract_references(&body).is_empty());
    }

    #[test]
    fn reduction_and_loss_are_distinct_states() {
        let elided = "a\n… [1000 bytes (~400 tokens) elided to fit the context] …\nb";
        assert!(result_is_reduced(elided));
        assert!(result_is_lost(elided, false));
        assert!(
            !result_is_lost(elided, true),
            "a saved artifact makes the elision recoverable, not lost"
        );
        let saved = format!(
            "{}\npreview",
            leveler_model::tool_output::saved_reference("/state/artifacts/cc.txt", "cc", 10)
        );
        assert!(result_is_reduced(&saved));
        assert!(!result_is_lost(&saved, true));
        assert!(!result_is_reduced("exit 0\nall tests passed"));
    }

    #[test]
    fn projection_serves_the_exact_model_result_and_typed_failures() {
        let root = std::env::temp_dir().join(format!(
            "leveler-tool-output-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ArtifactStore::new(&root);
        let stored = StoredToolResult {
            content: "行1\n行2\n".to_string(),
        };
        let output = project("c1", Some(&stored), None, 0, 1024, &store);
        assert_eq!(output.state, UiToolOutputState::Available);
        assert_eq!(output.text, stored.content);
        assert!(!output.has_more);

        let missing = project("c1", None, None, 0, 1024, &store);
        assert_eq!(missing.state, UiToolOutputState::Unavailable);
        assert_eq!(missing.unavailable, Some(UiToolOutputUnavailable::NotSaved));

        let unknown = project("c1", Some(&stored), Some(3), 0, 1024, &store);
        assert_eq!(
            unknown.unavailable,
            Some(UiToolOutputUnavailable::UnknownPart)
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
