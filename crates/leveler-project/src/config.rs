//! Per-repository configuration: `.leveler/config.yaml` (spec §37).
//!
//! This is what lets CodeLeveler work on *any* language/project: the repo
//! declares how to format/build/test itself, its default model and mode, and
//! extra ignore rules. Everything is optional; absent config falls back to
//! language-derived defaults.

use serde::Deserialize;

/// A program + argument vector.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Explicit verification commands for this repo. When present these override the
/// language-derived plan, so any toolchain works (pytest, gradle, cmake, ...).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct VerifySpec {
    /// Formatter (best-effort, non-gating).
    #[serde(default)]
    pub format: Option<CommandSpec>,
    /// Build/compile check (gating).
    #[serde(default)]
    pub build: Option<CommandSpec>,
    /// Test command (gating).
    #[serde(default)]
    pub test: Option<CommandSpec>,
}

/// Optional resource ceilings for one top-level Goal/Chat run. Round count
/// intentionally does not belong here. Token and cost ceilings are opt-in
/// (a cost cap additionally requires auditable pricing). Duration is opt-in too:
/// provider connection/idle timeouts protect infrastructure, while task lifetime
/// is a product/user policy that must not vary with gateway latency.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RunLimitsConfig {
    #[serde(default)]
    pub max_model_tokens: Option<u64>,
    #[serde(default)]
    pub max_cost_usd_micros: Option<u64>,
    #[serde(default)]
    pub max_duration_seconds: Option<u64>,
}

impl VerifySpec {
    pub fn is_empty(&self) -> bool {
        self.format.is_none() && self.build.is_none() && self.test.is_none()
    }
}

/// Multi-agent product settings under `agents:` in project config.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentsConfig {
    /// When false, `spawn_agent` is not advertised. Default true when omitted.
    #[serde(default = "default_true")]
    pub delegation: bool,
    /// Whether the harness launches an independent reviewer at closure.
    /// `off` (default) never launches; `required` launches after any product
    /// mutation. Explicit only — the runtime never infers a review from the
    /// shape of the change. Legacy values `auto` (reads as `off`) and
    /// `always` (reads as `required`) are still accepted.
    #[serde(default)]
    pub independent_review: IndependentReview,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            delegation: true,
            independent_review: IndependentReview::default(),
        }
    }
}

/// Whether the harness launches an independent reviewer (see [`AgentsConfig`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndependentReview {
    /// Never. The product default; `auto` (the deleted shape trigger) reads
    /// as this.
    #[default]
    #[serde(alias = "auto")]
    Off,
    /// After any product mutation. `always` is the legacy spelling.
    #[serde(alias = "always")]
    Required,
}

fn default_true() -> bool {
    true
}

/// `memory:` — whether durable project memory is part of this project at all.
///
/// Default is ON. When disabled, loading memory is not permitted; existing
/// durable entries remain stored. Tool exposure is controlled per goal.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MemoryConfig {
    /// When false: no turn is admitted to memory extraction, no background
    /// provider request runs, no memory catalog/recall reaches the model, and
    /// the memory tools are not exposed. Existing memory data is NOT deleted.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// User facts: preferences, decisions, notes, corrections and the
    /// `remember` path. Only effective while `enabled` is also true.
    #[serde(default = "default_true")]
    pub user_enabled: bool,
    /// Agent experience: automatic extraction and recall of evidence-bound
    /// lessons. Only effective while `enabled` is also true. Disabling it
    /// stops extraction and recall but never deletes existing experience.
    /// OFF by default: an absent key, including in YAML written before this
    /// lane existed, means experimental lessons stay closed until the project
    /// opts in with an explicit `true`.
    #[serde(default)]
    pub experience_enabled: bool,
    /// Removed `backend` selector (Memory 2.0 cutover).
    ///
    /// Markdown is the only implementation, so this key carries no authority.
    /// It is still accepted — and reported by [`Self::deprecated_backend`] —
    /// because an existing `config.yaml` that names `legacy` or `markdown`
    /// must keep loading its other settings instead of silently losing them.
    #[serde(default, rename = "backend")]
    pub deprecated_backend: Option<String>,
}

impl MemoryConfig {
    /// A notice for a config that still carries the removed `backend` key.
    ///
    /// Callers show this to the user; the value never selects a store, so an
    /// old `legacy` line cannot reactivate the deleted backend.
    pub fn deprecated_backend(&self) -> Option<String> {
        let value = self.deprecated_backend.as_deref()?.trim();
        Some(format!(
            "memory.backend = \"{value}\" 已移除：Markdown Memory 2.0 是唯一实现。请从 .leveler/config.yaml 删除该字段。"
        ))
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            user_enabled: true,
            experience_enabled: false,
            deprecated_backend: None,
        }
    }
}

/// `rules:` — deterministic delivery policy for project-rule documents.
///
/// Rule documents are authority-bearing, so their DELIVERY is decided by the
/// runtime, never by a model. This block names which sections are MANDATORY
/// (heading substrings, matched case-insensitively) and the one byte budget
/// every rule document of a request shares. Anything else is optional and
/// fills what the budget leaves; anything undelivered is named in the prompt
/// with its source and section id so `read_project_rules` can return it
/// verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RulesConfig {
    /// Heading or section-id substrings that are always delivered first, in
    /// document order. An empty list means no section is mandatory and
    /// document order fills the budget.
    #[serde(default)]
    pub always_delivered: Vec<String>,
    /// Request-wide delivery budget in bytes (all rule documents together).
    /// `None` uses the loader's built-in default.
    #[serde(default)]
    pub budget_bytes: Option<usize>,
}

/// The parsed `.leveler/config.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectConfig {
    /// Default model reference (e.g. `deepseek/deepseek-flash`).
    #[serde(default)]
    pub model: Option<String>,
    /// Default permission profile (`request_approval` | `assisted` | `full_access`).
    #[serde(default)]
    pub mode: Option<String>,
    /// Explicit verification commands.
    #[serde(default)]
    pub verify: VerifySpec,
    /// Optional token/cost/time ceilings. Absence means run until terminal.
    #[serde(default)]
    pub limits: RunLimitsConfig,
    /// Multi-agent delegation settings.
    #[serde(default)]
    pub agents: AgentsConfig,
    /// Durable project memory. Absent = enabled (the historical behaviour).
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Deterministic project-rule delivery policy.
    #[serde(default)]
    pub rules: RulesConfig,
    /// Extra ignore globs.
    #[serde(default)]
    pub ignore: Vec<String>,
}

/// The result of reading the rule delivery policy: the policy itself plus why
/// it could not be read when it could not.
///
/// `.leveler/config.yaml` is a whole-project file, so one malformed key
/// (a `limits:` typo, say) used to make the rule policy vanish in silence. For
/// a non-authority setting that is fine; for the policy that decides which
/// project rules are mandatory it is not, so the reason travels with the
/// value.
pub struct LoadedRulesPolicy {
    pub policy: RulesConfig,
    pub diagnostic: Option<String>,
}

impl ProjectConfig {
    /// Load `<root>/.leveler/config.yaml`, or `None` if it is absent or invalid
    /// (invalid config is non-fatal — the tool falls back to defaults).
    pub fn load(root: &std::path::Path) -> Option<Self> {
        let path = root.join(".leveler/config.yaml");
        let raw = std::fs::read_to_string(path).ok()?;
        serde_yaml::from_str(&raw).ok()
    }

    /// The rule delivery policy, with the reason when the file exists but
    /// cannot be parsed. An absent file is not a diagnostic: a project without
    /// `.leveler/config.yaml` simply has no mandatory declarations.
    pub fn load_rules_policy(root: &std::path::Path) -> LoadedRulesPolicy {
        let path = root.join(".leveler/config.yaml");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return LoadedRulesPolicy {
                policy: RulesConfig::default(),
                diagnostic: None,
            };
        };
        match serde_yaml::from_str::<ProjectConfig>(&raw) {
            Ok(config) => LoadedRulesPolicy {
                policy: config.rules,
                diagnostic: None,
            },
            Err(error) => LoadedRulesPolicy {
                policy: RulesConfig::default(),
                diagnostic: Some(format!(".leveler/config.yaml: {error}")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Top-level resource ceilings are opt-in. Provider request/idle timeouts
    /// protect infrastructure calls; a task wall-clock limit is a separate user
    /// policy and must not make a slow gateway change semantic task outcomes.
    #[test]
    fn an_absent_limits_block_has_no_task_lifetime_cap() {
        let from_default = RunLimitsConfig::default();
        assert_eq!(
            from_default.max_duration_seconds, None,
            "an unconfigured run must continue until a semantic terminal state"
        );

        // A config file that omits `limits:` entirely stays unbounded too.
        let cfg: ProjectConfig = serde_yaml::from_str("model: deepseek/deepseek-v4-pro\n").unwrap();
        assert_eq!(cfg.limits.max_duration_seconds, None);

        // Token and cost ceilings stay opt-in.
        assert_eq!(from_default.max_model_tokens, None);
        assert_eq!(from_default.max_cost_usd_micros, None);
    }

    /// The user can still opt into an explicit task lifetime ceiling.
    #[test]
    fn an_explicit_duration_is_preserved() {
        let cfg: ProjectConfig =
            serde_yaml::from_str("model: m/m\nlimits:\n  max_duration_seconds: 120\n").unwrap();
        assert_eq!(cfg.limits.max_duration_seconds, Some(120));
    }

    #[test]
    fn parses_verify_and_model() {
        let yaml = r#"
model: deepseek/deepseek-v4-pro
mode: assisted
limits:
  max_model_tokens: 200000
  max_cost_usd_micros: 500000
  max_duration_seconds: 7200
verify:
  format: { program: black, args: ["."] }
  build:  { program: python, args: ["-m", "compileall", "src"] }
  test:   { program: pytest, args: ["-q"] }
ignore:
  - "*.log"
"#;
        let cfg: ProjectConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(cfg.mode.as_deref(), Some("assisted"));
        assert_eq!(cfg.limits.max_model_tokens, Some(200_000));
        assert_eq!(cfg.limits.max_cost_usd_micros, Some(500_000));
        assert_eq!(cfg.limits.max_duration_seconds, Some(7200));
        assert_eq!(cfg.verify.test.as_ref().unwrap().program, "pytest");
        assert!(!cfg.verify.is_empty());
        assert_eq!(cfg.ignore, vec!["*.log"]);
    }

    #[test]
    fn empty_config_is_all_none() {
        let cfg: ProjectConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.verify.is_empty());
        assert!(cfg.model.is_none());
        assert!(
            cfg.agents.delegation,
            "delegation defaults on when agents: is omitted"
        );
    }

    #[test]
    fn agents_delegation_can_be_disabled() {
        let cfg: ProjectConfig = serde_yaml::from_str("agents:\n  delegation: false\n").unwrap();
        assert!(!cfg.agents.delegation);
    }

    #[test]
    fn independent_review_defaults_to_off_and_reads_legacy_values() {
        let cfg: ProjectConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(cfg.agents.independent_review, IndependentReview::Off);
        let off: ProjectConfig =
            serde_yaml::from_str("agents:\n  independent_review: off\n").unwrap();
        assert_eq!(off.agents.independent_review, IndependentReview::Off);
        let required: ProjectConfig =
            serde_yaml::from_str("agents:\n  independent_review: required\n").unwrap();
        assert_eq!(
            required.agents.independent_review,
            IndependentReview::Required
        );
        // Legacy spellings from before the shape trigger was deleted.
        let auto: ProjectConfig =
            serde_yaml::from_str("agents:\n  independent_review: auto\n").unwrap();
        assert_eq!(auto.agents.independent_review, IndependentReview::Off);
        let always: ProjectConfig =
            serde_yaml::from_str("agents:\n  independent_review: always\n").unwrap();
        assert_eq!(
            always.agents.independent_review,
            IndependentReview::Required
        );
    }

    #[test]
    fn memory_is_enabled_by_default_and_can_be_switched_off() {
        // An absent block keeps the behaviour every existing session had.
        let cfg: ProjectConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.memory.enabled);
        let default = ProjectConfig::default();
        assert!(default.memory.enabled);

        // An explicit block wins.
        let off: ProjectConfig = serde_yaml::from_str("memory:\n  enabled: false\n").unwrap();
        assert!(!off.memory.enabled);
        let on: ProjectConfig = serde_yaml::from_str("memory:\n  enabled: true\n").unwrap();
        assert!(on.memory.enabled);
    }

    #[test]
    fn experience_is_off_by_default_and_user_facts_stay_on() {
        // Automatic experience is an experimental lane. A project that never
        // opted in keeps user facts and does not get lessons extracted behind
        // its back; a missing key means closed, not "inherit a historical on".
        let absent: ProjectConfig = serde_yaml::from_str("{}").unwrap();
        assert!(absent.memory.enabled);
        assert!(absent.memory.user_enabled);
        assert!(!absent.memory.experience_enabled);
        let default = ProjectConfig::default();
        assert!(default.memory.user_enabled);
        assert!(!default.memory.experience_enabled);

        // Old YAML has only `enabled`; user facts keep the historical
        // behaviour while experience stays closed until explicitly enabled.
        let old: ProjectConfig = serde_yaml::from_str("memory:\n  enabled: true\n").unwrap();
        assert!(old.memory.user_enabled);
        assert!(!old.memory.experience_enabled);

        // An explicit opt-in still enables experience.
        let opted_in: ProjectConfig =
            serde_yaml::from_str("memory:\n  experience_enabled: true\n").unwrap();
        assert!(opted_in.memory.user_enabled);
        assert!(opted_in.memory.experience_enabled);

        let no_experience: ProjectConfig =
            serde_yaml::from_str("memory:\n  experience_enabled: false\n").unwrap();
        assert!(no_experience.memory.user_enabled);
        assert!(!no_experience.memory.experience_enabled);

        let no_user: ProjectConfig =
            serde_yaml::from_str("memory:\n  user_enabled: false\n").unwrap();
        assert!(!no_user.memory.user_enabled);
        assert!(!no_user.memory.experience_enabled);

        // The total switch owns both lanes; a lane value cannot reopen it.
        let total_off: ProjectConfig = serde_yaml::from_str(
            "memory:\n  enabled: false\n  user_enabled: true\n  experience_enabled: true\n",
        )
        .unwrap();
        assert!(!total_off.memory.enabled);
        assert!(total_off.memory.user_enabled);
        assert!(total_off.memory.experience_enabled);
    }

    #[test]
    fn a_removed_backend_key_is_accepted_and_reported() {
        // Markdown is the only implementation. A config written against the
        // two-backend era must still load (so unrelated settings survive) and
        // must say that the key no longer selects anything.
        let legacy: ProjectConfig = serde_yaml::from_str(
            "memory:\n  backend: legacy\n  enabled: true\n  experience_enabled: true\n",
        )
        .unwrap();
        assert!(legacy.memory.enabled);
        assert!(legacy.memory.experience_enabled);
        let notice = legacy.memory.deprecated_backend().unwrap();
        assert!(notice.contains("memory.backend"), "{notice}");
        assert!(notice.contains("legacy"), "{notice}");
        assert!(notice.contains("Markdown"), "{notice}");

        // Nothing selects a store: there is no field left to read it from.
        let modern: ProjectConfig = serde_yaml::from_str("memory:\n  backend: markdown\n").unwrap();
        assert!(modern.memory.deprecated_backend().is_some());
        let clean: ProjectConfig = serde_yaml::from_str("memory:\n  enabled: true\n").unwrap();
        assert!(clean.memory.deprecated_backend().is_none());
    }

    #[test]
    fn rules_delivery_policy_is_optional_and_deterministic() {
        let cfg: ProjectConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.rules.always_delivered.is_empty());
        assert_eq!(cfg.rules.budget_bytes, None);
        let cfg: ProjectConfig = serde_yaml::from_str(
            "rules:\n  always_delivered:\n    - 架构门禁\n  budget_bytes: 16000\n",
        )
        .unwrap();
        assert_eq!(cfg.rules.always_delivered, vec!["架构门禁".to_string()]);
        assert_eq!(cfg.rules.budget_bytes, Some(16000));
    }

    #[test]
    fn tolerates_unknown_fields() {
        // Spec §37 config has more keys; we parse the subset we use.
        let yaml = "version: 1\npermissions:\n  allow: [read]\nmodel: x/y\n";
        let cfg: ProjectConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("x/y"));
    }
}
