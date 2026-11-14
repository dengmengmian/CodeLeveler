//! Bounded JSON rendering of one captured turn range.
//!
//! The extractor needs enough of the turn to tell a finished result from
//! abandoned work, and nothing else. Condensation is staged so the least
//! useful bytes go first: reasoning and images, then tool bodies, then the
//! middle of a long tool loop, and only last the user's own words. It never
//! fails on size; a transcript that is still over budget is reported as such
//! in the stats the caller attaches to the prompt.
//!
//! The runtime converts stored [`leveler_model::Message`] rows into these
//! neutral items in the application layer. This crate owns the algorithm and
//! the byte limits, not the storage schema.

use serde::{Deserialize, Serialize};

/// ~130k tokens of JSON; leaves room for the extractor's output in the context window.
pub const BASE_MAX_TRANSCRIPT_BYTES: usize = 512 * 1024;
pub const BASE_MAX_TRANSCRIPT_ITEMS: usize = 512;
const MIN_MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MIN_MAX_TRANSCRIPT_ITEMS: usize = 64;

const TOOL_RESULT_TIERS: [usize; 3] = [4 * 1024, 1024, 256];
const TOOL_ARGS_TIERS: [usize; 3] = [2 * 1024, 512, 256];
const KEEP_EDGE_START: usize = 8;
const USER_TEXT_FLOOR_BYTES: usize = 4 * 1024;

/// One durable transcript step, shaped for the extractor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TranscriptItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<TranscriptToolCall>,
    },
    ToolResult {
        tool: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    System {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Image {
        media_type: String,
        byte_len: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptToolCall {
    pub name: String,
    pub arguments: String,
}

impl TranscriptItem {
    fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptBudget {
    pub max_bytes: usize,
    pub max_items: usize,
}

impl TranscriptBudget {
    /// Near-budget first attempt; each retry halves the ceiling down to a floor.
    pub fn for_attempt(attempt: u32) -> Self {
        let shift = attempt.saturating_sub(1).min(8);
        Self {
            max_bytes: (BASE_MAX_TRANSCRIPT_BYTES >> shift).max(MIN_MAX_TRANSCRIPT_BYTES),
            max_items: (BASE_MAX_TRANSCRIPT_ITEMS >> shift).max(MIN_MAX_TRANSCRIPT_ITEMS),
        }
    }

    fn fits(self, json: &str, items: usize) -> bool {
        json.len() <= self.max_bytes && items <= self.max_items
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CondensationStats {
    pub original_bytes: usize,
    pub original_items: usize,
    pub final_bytes: usize,
    pub final_items: usize,
    pub reasoning_dropped: usize,
    pub images_dropped: usize,
    pub tool_results_trimmed: usize,
    pub tool_args_trimmed: usize,
    pub steps_omitted: usize,
    pub user_text_trimmed: usize,
    pub over_budget: bool,
}

impl CondensationStats {
    pub fn is_condensed(&self) -> bool {
        self.reasoning_dropped > 0
            || self.images_dropped > 0
            || self.tool_results_trimmed > 0
            || self.tool_args_trimmed > 0
            || self.steps_omitted > 0
            || self.user_text_trimmed > 0
    }

    /// Tell the extractor what was removed, so it does not treat a marker as
    /// something a participant wrote.
    pub fn prompt_note(&self) -> Option<String> {
        if !self.is_condensed() {
            return None;
        }
        let mut parts = Vec::new();
        if self.reasoning_dropped > 0 {
            parts.push("model reasoning omitted".to_owned());
        }
        if self.images_dropped > 0 {
            parts.push("images omitted".to_owned());
        }
        if self.tool_results_trimmed > 0 {
            parts.push("tool outputs truncated".to_owned());
        }
        if self.tool_args_trimmed > 0 {
            parts.push("tool arguments truncated".to_owned());
        }
        if self.steps_omitted > 0 {
            parts.push(format!("{} intermediate steps omitted", self.steps_omitted));
        }
        if self.user_text_trimmed > 0 {
            parts.push("user text truncated".to_owned());
        }
        Some(format!(
            "The transcript was condensed to fit the extraction budget ({}). \
             Text between \"[...\" and \"...]\" markers was removed, not written by a participant.",
            parts.join(", ")
        ))
    }
}

#[derive(Debug)]
pub struct CondensedTranscript {
    pub json: String,
    pub stats: CondensationStats,
}

/// Condense a turn transcript to the extraction budget. Never fails on size.
pub fn condense_turn_transcript(
    mut items: Vec<TranscriptItem>,
    budget: TranscriptBudget,
) -> Result<CondensedTranscript, String> {
    let mut stats = CondensationStats {
        original_items: items.len(),
        ..CondensationStats::default()
    };
    let mut json = serialize(&items)?;
    stats.original_bytes = json.len();
    if budget.fits(&json, items.len()) {
        return Ok(finish(json, items.len(), stats));
    }

    let before = items.len();
    items.retain(|item| !matches!(item, TranscriptItem::Reasoning { .. }));
    stats.reasoning_dropped = before - items.len();
    let mut replaced = Vec::with_capacity(items.len());
    for item in items {
        match item {
            TranscriptItem::Image { .. } => {
                stats.images_dropped += 1;
                replaced.push(TranscriptItem::System {
                    text: "[... image omitted ...]".to_owned(),
                });
            }
            other => replaced.push(other),
        }
    }
    items = replaced;
    json = serialize(&items)?;
    if budget.fits(&json, items.len()) {
        return Ok(finish(json, items.len(), stats));
    }

    for (result_cap, args_cap) in TOOL_RESULT_TIERS.into_iter().zip(TOOL_ARGS_TIERS) {
        for item in &mut items {
            match item {
                TranscriptItem::ToolResult { content, .. } => {
                    if let Some(trimmed) = trim_middle(content, result_cap) {
                        *content = trimmed;
                        stats.tool_results_trimmed += 1;
                    }
                }
                TranscriptItem::Assistant { tool_calls, .. } => {
                    for call in tool_calls {
                        if let Some(trimmed) = trim_middle(&call.arguments, args_cap) {
                            call.arguments = trimmed;
                            stats.tool_args_trimmed += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        json = serialize(&items)?;
        if budget.fits(&json, items.len()) {
            return Ok(finish(json, items.len(), stats));
        }
    }

    let mut keep_edge = KEEP_EDGE_START;
    loop {
        let (reduced, omitted) = omit_middle_steps(&items, keep_edge);
        let reduced_json = serialize(&reduced)?;
        if budget.fits(&reduced_json, reduced.len()) || keep_edge == 1 {
            let final_items = reduced.len();
            items = reduced;
            json = reduced_json;
            stats.steps_omitted = omitted;
            if budget.fits(&json, final_items) {
                return Ok(finish(json, final_items, stats));
            }
            break;
        }
        keep_edge /= 2;
    }

    let user_count = items.iter().filter(|item| item.is_user()).count().max(1);
    let other_bytes: usize = items
        .iter()
        .filter(|item| !item.is_user())
        .map(|item| serde_json::to_string(item).map(|s| s.len()).unwrap_or(0))
        .sum();
    let mut share = budget
        .max_bytes
        .saturating_sub(other_bytes)
        .checked_div(user_count)
        .unwrap_or(0)
        .max(USER_TEXT_FLOOR_BYTES);
    loop {
        let mut trimmed_any = false;
        for item in &mut items {
            if let TranscriptItem::User { text } = item
                && let Some(trimmed) = trim_middle(text, share)
            {
                *text = trimmed;
                trimmed_any = true;
            }
        }
        if trimmed_any {
            stats.user_text_trimmed += 1;
        }
        json = serialize(&items)?;
        if budget.fits(&json, items.len()) || share <= USER_TEXT_FLOOR_BYTES {
            break;
        }
        // JSON escaping and the envelope cost bytes the share did not count.
        share = (share * 9 / 10).max(USER_TEXT_FLOOR_BYTES);
    }
    stats.over_budget = !budget.fits(&json, items.len());
    Ok(finish(json, items.len(), stats))
}

fn finish(json: String, items: usize, mut stats: CondensationStats) -> CondensedTranscript {
    stats.final_bytes = json.len();
    stats.final_items = items;
    CondensedTranscript { json, stats }
}

fn serialize(items: &[TranscriptItem]) -> Result<String, String> {
    serde_json::to_string(items)
        .map_err(|error| format!("durable transcript encoding failed: {error}"))
}

fn trim_middle(text: &str, max_bytes: usize) -> Option<String> {
    if text.len() <= max_bytes {
        return None;
    }
    let head_len = max_bytes * 2 / 3;
    let tail_len = max_bytes.saturating_sub(head_len);
    let head_end = crate::text_utils::floor_char_boundary(text, head_len);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_len)).max(head_end);
    let head = text.get(..head_end).unwrap_or_default();
    let tail = text.get(tail_start..).unwrap_or_default();
    let omitted = text.len().saturating_sub(head.len() + tail.len());
    Some(format!("{head}\n[... {omitted} bytes omitted ...]\n{tail}"))
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn omit_middle_steps(items: &[TranscriptItem], keep_edge: usize) -> (Vec<TranscriptItem>, usize) {
    let step_positions: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| !item.is_user())
        .map(|(index, _)| index)
        .collect();
    if step_positions.len() <= keep_edge * 2 {
        return (items.to_vec(), 0);
    }
    let (Some(&first_dropped), Some(&last_dropped)) = (
        step_positions.get(keep_edge),
        step_positions.get(step_positions.len().saturating_sub(keep_edge + 1)),
    ) else {
        return (items.to_vec(), 0);
    };
    let mut reduced = Vec::with_capacity(items.len());
    let mut omitted = 0;
    let mut marker_placed = false;
    for (index, item) in items.iter().enumerate() {
        let is_step = !item.is_user();
        if is_step && (first_dropped..=last_dropped).contains(&index) {
            omitted += 1;
            if !marker_placed {
                reduced.push(TranscriptItem::Assistant {
                    text: "[... intermediate steps omitted ...]".to_owned(),
                    tool_calls: Vec::new(),
                });
                marker_placed = true;
            }
            continue;
        }
        reduced.push(item.clone());
    }
    for item in &mut reduced {
        if let TranscriptItem::Assistant { text, .. } = item
            && text == "[... intermediate steps omitted ...]"
        {
            *text = format!("[... {omitted} intermediate steps omitted ...]");
            break;
        }
    }
    (reduced, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_loop(steps: usize, result_bytes: usize) -> Vec<TranscriptItem> {
        let mut items = vec![TranscriptItem::User {
            text: "please refactor the workflow modal".to_owned(),
        }];
        for step in 0..steps {
            items.push(TranscriptItem::Reasoning {
                text: format!("thinking about step {step}"),
            });
            items.push(TranscriptItem::Assistant {
                text: format!("step {step}"),
                tool_calls: vec![TranscriptToolCall {
                    name: "read_file".to_owned(),
                    arguments: format!("{{\"path\":\"src/file{step}.rs\"}}"),
                }],
            });
            items.push(TranscriptItem::ToolResult {
                tool: "read_file".to_owned(),
                content: "x".repeat(result_bytes),
                is_error: false,
            });
        }
        items.push(TranscriptItem::Assistant {
            text: "Done: the modal now confirms before deleting.".to_owned(),
            tool_calls: Vec::new(),
        });
        items
    }

    #[test]
    fn under_budget_turn_is_byte_identical() {
        let items = tool_loop(3, 100);
        let plain = serde_json::to_string(&items).unwrap();
        let condensed = condense_turn_transcript(items, TranscriptBudget::for_attempt(1)).unwrap();
        assert_eq!(condensed.json, plain);
        assert!(!condensed.stats.is_condensed());
        assert!(condensed.stats.prompt_note().is_none());
    }

    #[test]
    fn budget_shrinks_with_attempts_but_never_below_the_floor() {
        let first = TranscriptBudget::for_attempt(1);
        let second = TranscriptBudget::for_attempt(2);
        assert_eq!(first.max_bytes, BASE_MAX_TRANSCRIPT_BYTES);
        assert_eq!(second.max_bytes, BASE_MAX_TRANSCRIPT_BYTES / 2);
        let floor = TranscriptBudget::for_attempt(100);
        assert_eq!(floor.max_bytes, MIN_MAX_TRANSCRIPT_BYTES);
        assert_eq!(floor.max_items, MIN_MAX_TRANSCRIPT_ITEMS);
    }

    #[test]
    fn oversized_tool_loop_trims_tool_bodies_before_user_text() {
        let items = tool_loop(8, 32 * 1024);
        let budget = TranscriptBudget {
            max_bytes: 24 * 1024,
            max_items: 512,
        };
        let condensed = condense_turn_transcript(items, budget).unwrap();
        assert!(condensed.stats.tool_results_trimmed > 0);
        assert_eq!(condensed.stats.user_text_trimmed, 0);
        assert!(
            condensed
                .json
                .contains("please refactor the workflow modal"),
            "the user's own words survive while tool bodies absorb the cut"
        );
        assert!(condensed.json.len() < 64 * 1024);
    }

    #[test]
    fn long_tool_loop_omits_middle_steps_with_a_marker() {
        let items = tool_loop(200, 64);
        let budget = TranscriptBudget {
            max_bytes: 8 * 1024,
            max_items: 40,
        };
        let condensed = condense_turn_transcript(items, budget).unwrap();
        assert!(condensed.stats.steps_omitted > 0);
        assert!(condensed.json.contains("intermediate steps omitted"));
    }

    #[test]
    fn reasoning_and_images_never_reach_the_extractor() {
        let items = vec![
            TranscriptItem::User {
                text: "look at this".to_owned(),
            },
            TranscriptItem::Image {
                media_type: "image/png".to_owned(),
                byte_len: 10,
            },
            TranscriptItem::Reasoning {
                text: "private chain of thought".to_owned(),
            },
        ];
        let budget = TranscriptBudget {
            max_bytes: 1,
            max_items: 1,
        };
        let condensed = condense_turn_transcript(items, budget).unwrap();
        assert_eq!(condensed.stats.reasoning_dropped, 1);
        assert_eq!(condensed.stats.images_dropped, 1);
        assert!(!condensed.json.contains("private chain of thought"));
        assert!(!condensed.json.contains("image/png"));
    }

    #[test]
    fn trim_middle_keeps_both_edges_and_respects_utf8_boundaries() {
        let text = format!("{}é{}", "a".repeat(20), "b".repeat(20));
        let trimmed = trim_middle(&text, 21).unwrap();
        assert!(trimmed.starts_with('a'));
        assert!(trimmed.ends_with('b'));
        assert!(trimmed.contains("bytes omitted"));
    }

    #[test]
    fn condensation_never_fails_on_size() {
        let items = vec![TranscriptItem::User {
            text: "x".repeat(4096),
        }];
        let budget = TranscriptBudget {
            max_bytes: 1,
            max_items: 1,
        };
        let condensed = condense_turn_transcript(items, budget).unwrap();
        assert!(condensed.stats.over_budget);
    }
}
