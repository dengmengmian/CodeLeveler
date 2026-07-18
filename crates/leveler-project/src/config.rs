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

/// The parsed `.leveler/config.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectConfig {
    /// Default model reference (e.g. `deepseek/deepseek-v4-pro`).
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
    /// Extra ignore globs.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Additional directories the agent may **read** (absolute or repo-relative).
    /// Writes remain confined to the primary workspace root.
    #[serde(default)]
    pub readonly_roots: Vec<String>,
}

impl ProjectConfig {
    /// Load `<root>/.leveler/config.yaml`, or `None` if it is absent or invalid
    /// (invalid config is non-fatal — the tool falls back to defaults).
    pub fn load(root: &std::path::Path) -> Option<Self> {
        let path = root.join(".leveler/config.yaml");
        let raw = std::fs::read_to_string(path).ok()?;
        serde_yaml::from_str(&raw).ok()
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
    }

    #[test]
    fn tolerates_unknown_fields() {
        // Spec §37 config has more keys; we parse the subset we use.
        let yaml = "version: 1\npermissions:\n  allow: [read]\nmodel: x/y\n";
        let cfg: ProjectConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("x/y"));
    }
}
