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
    /// Valid, trusted, and enabled: the entry can be loaded.
    Available,
    /// Valid and trusted, but the user disabled it; the package stays listed
    /// so it can be re-enabled.
    Disabled,
    /// The definition is broken; it cannot be loaded anywhere.
    Invalid,
    /// A package exists but its exact bytes have no human approval (or the
    /// approved version differs). It is listed so the user can inspect it,
    /// but it is never loaded without a decision. Carries no description:
    /// unapproved metadata must not be presented as usable instruction.
    Untrusted,
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
    /// Why it is disabled or invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Present only for an available entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Display-only argument shape from `SKILL.md` frontmatter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Whether `/` and `$` may invoke this Skill.
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    /// Whether the model index and `load_skill` may use this Skill.
    #[serde(default)]
    pub disable_model_invocation: bool,
    /// Whether the bare `/name` form is owned by a product command, so the
    /// Skill's slash identity is the qualified `/<scope>:<name>`.
    #[serde(default)]
    pub slash_reserved: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadowed: Vec<UiShadowedSkill>,
}

fn default_true() -> bool {
    true
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
    /// `None` for an invalid entry. For an untrusted entry this is a bounded
    /// preview of the exact package, for human inspection only: the runtime
    /// never delivers it to the model before a trust decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Whether `body` omitted any package content. A truncated preview must
    /// not be presented as the complete definition.
    #[serde(default)]
    pub body_truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_files: Vec<String>,
}

/// Why a selected skill did not reach the turn context. Stable wire codes;
/// the display copy is the client's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiSkillDeliveryFailure {
    NotFound,
    Invalid,
    Disabled,
    Untrusted,
    UserInvocationDisabled,
    ModelInvocationDisabled,
    Read,
    Shadowed,
    CapabilityUnavailable,
}

/// One skill the user's message selected this turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSkillDeliveryEntry {
    pub name: String,
    /// The token the user typed (`/deploy`, `$deploy`, or both).
    pub requested_as: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<UiSkillScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<UiSkillSource>,
    /// Procedure body bytes; zero when nothing was loaded.
    pub bytes: usize,
    /// `None` means the package was loaded into the turn context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<UiSkillDeliveryFailure>,
}

/// The live projection of a turn's skill delivery. The durable audit is the
/// engine event; this is the compact fact a client shows while the turn runs.
/// It carries no procedure body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSkillDelivery {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<UiSkillDeliveryEntry>,
    /// Set when the host refused the whole delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<UiSkillDeliveryFailure>,
}
