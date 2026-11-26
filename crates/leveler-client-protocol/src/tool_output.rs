//! Read-only access to a tool call's full recorded result.
//!
//! The model-visible tool result is persisted in the session's transcript. When
//! it exceeded the per-result bound, the runtime saved the complete text to its
//! content-addressed artifact store and replaced the body with a bounded
//! preview plus a reference. These types let a client page the recorded result
//! and each complete artifact WITHOUT ever naming a path: the runtime resolves
//! references from the stored result itself, and the request carries only the
//! call id and an index.

use serde::{Deserialize, Serialize};

/// Largest page a client may ask for. The runtime clamps to this, so one
/// response can never become an unbounded transcript read.
pub const TOOL_OUTPUT_PAGE_MAX_BYTES: u32 = 256 * 1024;

/// Page used when a request does not state one.
pub const TOOL_OUTPUT_PAGE_DEFAULT_BYTES: u32 = 64 * 1024;

/// One page of a tool call's recorded output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiToolOutput {
    /// The tool call this page belongs to (echo of the request).
    pub call_id: String,
    pub state: UiToolOutputState,
    /// Why the output is unavailable. Present only with
    /// [`UiToolOutputState::Unavailable`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<UiToolOutputUnavailable>,
    /// Which recorded part this page belongs to.
    pub part: UiToolOutputPart,
    /// Human-readable name of the part (`模型所见结果`, `stdout`, ...).
    pub label: String,
    /// This page's text. Empty when unavailable.
    pub text: String,
    /// Length of the whole part in bytes.
    pub total_bytes: u64,
    /// Byte offset of this page in the whole part.
    pub offset: u64,
    /// Offset of the next page; equals `total_bytes` at the end.
    pub next_offset: u64,
    pub has_more: bool,
    /// The model-visible result was reduced: its preview is not the whole
    /// result. The complete text, when it exists, is among `references`.
    pub model_result_reduced: bool,
    /// The result was reduced with no saved copy: the original bytes are gone
    /// and no operation can recover them.
    pub full_output_lost: bool,
    /// Complete artifacts the recorded result references, in stable order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<UiToolOutputReference>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiToolOutputState {
    Available,
    Unavailable,
}

/// Why a recorded output could not be served. Each variant names a real state
/// of the durable record, never an inference about what the tool "should" have
/// produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiToolOutputUnavailable {
    /// No stored result exists for this call: it is still running, the
    /// transcript that held it was compacted away, or it was produced by a
    /// runtime that did not persist tool results.
    NotSaved,
    /// The recorded result names an artifact whose file is gone (quota sweep
    /// or manual deletion). The recorded preview remains what the model saw.
    ArtifactMissing,
    /// The request named a reference index the recorded result does not have.
    UnknownPart,
    /// The stored artifact could not be read for an environmental reason.
    ReadFailed,
}

/// Which recorded part a page belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiToolOutputPart {
    /// The exact result text the model received.
    Result,
    /// The complete text saved at `index` in `references`.
    Artifact { index: u32 },
}

/// One complete artifact a recorded result references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiToolOutputReference {
    /// Stable index a client passes back to page this artifact.
    pub index: u32,
    pub label: String,
    /// Size of the saved file when it exists.
    pub size_bytes: u64,
    /// False when the file is missing (swept or deleted); the client shows the
    /// reference as unavailable instead of hiding that it existed.
    pub available: bool,
}
