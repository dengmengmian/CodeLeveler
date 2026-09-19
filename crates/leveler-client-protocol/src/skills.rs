//! Read model for the resolved skill registry.
//!
//! A client never walks the filesystem itself: it reads the registry the
//! runtime already resolved. The same entry answers "what does `$name` load",
//! "why is this skill invalid" and "which definition won", so the CLI, the TUI
//! and the Web surface cannot disagree.

use serde::{Deserialize, Serialize};

/// Where a skill lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiSkillScope {
    Project,
    User,
    Builtin,
}

/// Which ecosystem's package format a skill follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiSkillSource {
    Native,
    Codex,
    AgentSkills,
    Claude,
    Builtin,
}

/// Whether an entry can be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiSkillStatus {
    /// Valid, and the directory resolves.
    Available,
    /// The definition is broken; it cannot be loaded anywhere.
    Invalid,
}

/// A lower-precedence skill the active one hides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiShadowedSkill {
    pub scope: UiSkillScope,
    pub source: UiSkillSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// The reason that definition is unusable, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One resolvable skill name, as the registry resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSkillEntry {
    pub name: String,
    pub scope: UiSkillScope,
    pub source: UiSkillSource,
    /// The package directory; `None` for built-ins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub status: UiSkillStatus,
    /// Why it is invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Present only for an available entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadowed: Vec<UiShadowedSkill>,
}

/// Something under a skills directory that is not a loadable skill at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSkillProblem {
    pub scope: UiSkillScope,
    pub source: UiSkillSource,
    pub location: String,
    pub error: String,
}

/// One skill with its full body and bundled files, for an inspector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSkillDetail {
    pub entry: UiSkillEntry,
    /// `None` for an invalid entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_files: Vec<String>,
}
