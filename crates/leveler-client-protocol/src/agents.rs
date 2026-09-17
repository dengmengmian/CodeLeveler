//! Read and write model for declarative agent definitions.
//!
//! A client never touches the agent files itself: it lists and reads the
//! registry through [`crate::ClientCommand::ListAgents`] /
//! [`crate::ClientCommand::GetAgent`], and changes it through
//! `CreateAgent` / `UpdateAgent` / `DeleteAgent`, which the runtime validates
//! and writes atomically. The user's own command is the authorization, so
//! these are never model-callable.

use serde::{Deserialize, Serialize};

/// Where a definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiAgentSource {
    Project,
    User,
    Builtin,
}

/// Where a client may write a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiAgentScope {
    Project,
    User,
}

/// The runtime capability class an agent runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiAgentCapability {
    ReadOnly,
    Writer,
    ScopedWriter,
}

/// Whether an entry can be spawned here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiAgentStatus {
    /// Valid, and everything it names exists on this machine.
    Available,
    /// Valid, but something it names is missing here (model, skill, effort).
    Unavailable,
    /// The definition itself is broken; it cannot be spawned anywhere.
    Invalid,
}

/// A lower-precedence definition the active one hides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiShadowedAgent {
    pub source: UiAgentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// One resolvable agent name, as the registry resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiAgentEntry {
    pub name: String,
    pub source: UiAgentSource,
    /// The definition's directory; `None` for built-ins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub status: UiAgentStatus,
    /// Why it is unavailable or invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The fields below are `None`/empty for an invalid definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<UiAgentCapability>,
    /// One of the runtime's structural roles (default, explorer, worker,
    /// reviewer): not editable, and not overridable.
    #[serde(default)]
    pub structural: bool,
    /// Launched by the harness only; a model cannot spawn it.
    #[serde(default)]
    pub harness_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// `None`: the capability's full toolset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadowed: Vec<UiShadowedAgent>,
}

/// Something under an agents directory that is not an agent at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiAgentProblem {
    pub source: UiAgentSource,
    pub location: String,
    pub error: String,
}

/// One agent with its full definition, for an editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiAgentDetail {
    pub entry: UiAgentEntry,
    /// `None` for structural built-ins and invalid definitions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// A definition a client asks the runtime to write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiAgentDraft {
    pub name: String,
    pub description: String,
    pub capability: UiAgentCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_secs: Option<u64>,
    pub instructions: String,
}

/// The definition a running or settled child was spawned from, as resolved at
/// spawn. Deleting or editing the definition does not change it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiChildAgentIdentity {
    /// The agent's name, e.g. `security-reviewer`.
    pub name: String,
    /// `project`, `user` or `builtin`.
    pub source: String,
    pub capability: String,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

#[cfg(test)]
mod tests {
    use crate::RuntimeEvent;

    /// A child event written before agents existed still reads, with no
    /// identity: an old session must render, not fail to parse.
    #[test]
    fn a_child_event_without_an_agent_identity_still_parses() {
        let legacy = r#"{"type":"sub_agent_updated","id":"a1","nickname":"Curie","role":"explorer","done":false,"ok":false,"detail":"look","profile_id":"explorer","read_only":true}"#;
        match serde_json::from_str::<RuntimeEvent>(legacy).expect("legacy event parses") {
            RuntimeEvent::SubAgentUpdated { agent, .. } => assert!(agent.is_none()),
            other => panic!("unexpected: {other:?}"),
        }
        let legacy_child = r#"{"id":"a1","nickname":"Curie","role":"explorer","purpose":"look","state":"running"}"#;
        let child: crate::UiChildAgent =
            serde_json::from_str(legacy_child).expect("legacy child parses");
        assert!(child.agent.is_none());
    }
}
