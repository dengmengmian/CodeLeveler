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
    /// Deterministic project-rule delivery policy.
    #[serde(default)]
    pub rules: RulesConfig,
    /// Extra ignore globs.
    #[serde(default)]
    pub ignore: Vec<String>,
}

/// Why the rule delivery policy could not be read. Parse failures are
/// separate: the file was readable but its content was not valid config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesPolicyReadFailure {
    PermissionDenied,
    IsADirectory,
    InvalidUtf8,
    Io,
}

impl RulesPolicyReadFailure {
    pub fn as_key(self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission_denied",
            Self::IsADirectory => "not_a_file",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::Io => "io",
        }
    }
}

impl std::fmt::Display for RulesPolicyReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_key())
    }
}

/// A structured reason the rule policy is not in force. `Read` means the file
/// exists but its bytes could not be obtained; `Parse` means the bytes were
/// obtained but are not valid config. They are never collapsed into "absent".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesPolicyDiagnostic {
    Read {
        failure: RulesPolicyReadFailure,
        detail: String,
    },
    Parse {
        detail: String,
    },
}

impl RulesPolicyDiagnostic {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Read { failure, .. } => failure.as_key(),
            Self::Parse { .. } => "parse",
        }
    }
}

impl std::fmt::Display for RulesPolicyDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { failure, detail } => {
                write!(f, ".leveler/config.yaml: {failure} ({detail})")
            }
            Self::Parse { detail } => write!(f, ".leveler/config.yaml: {detail}"),
        }
    }
}

/// The result of reading the rule delivery policy: the policy itself, whether
/// the file exists, and why it could not be used when it could not.
///
/// `.leveler/config.yaml` is a whole-project file, so one malformed key
/// (a `limits:` typo, say) used to make the rule policy vanish in silence. For
/// a non-authority setting that is fine; for the policy that decides which
/// project rules are mandatory it is not, so the reason travels with the
/// value.
pub struct LoadedRulesPolicy {
    pub policy: RulesConfig,
    pub diagnostic: Option<RulesPolicyDiagnostic>,
    /// The file exists (even if unreadable). Distinguishes "project stated no
    /// mandatory rules" from "the host cannot know what it stated".
    pub present: bool,
}

impl ProjectConfig {
    /// Load `<root>/.leveler/config.yaml`, or `None` if it is absent or invalid
    /// (invalid config is non-fatal — the tool falls back to defaults).
    pub fn load(root: &std::path::Path) -> Option<Self> {
        let path = root.join(".leveler/config.yaml");
        let raw = std::fs::read_to_string(path).ok()?;
        serde_yaml::from_str(&raw).ok()
    }

    /// The rule delivery policy, with a structured reason when the file exists
    /// but cannot be read or parsed. An absent file is not a diagnostic: a
    /// project without `.leveler/config.yaml` simply has no mandatory
    /// declarations.
    pub fn load_rules_policy(root: &std::path::Path) -> LoadedRulesPolicy {
        let path = root.join(".leveler/config.yaml");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return LoadedRulesPolicy {
                    policy: RulesConfig::default(),
                    diagnostic: None,
                    present: false,
                };
            }
            Err(error) => {
                let failure = match error.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        RulesPolicyReadFailure::PermissionDenied
                    }
                    std::io::ErrorKind::IsADirectory => RulesPolicyReadFailure::IsADirectory,
                    _ => RulesPolicyReadFailure::Io,
                };
                return LoadedRulesPolicy {
                    policy: RulesConfig::default(),
                    diagnostic: Some(RulesPolicyDiagnostic::Read {
                        failure,
                        detail: error.to_string(),
                    }),
                    present: true,
                };
            }
        };
        let raw = match String::from_utf8(bytes) {
            Ok(raw) => raw,
            Err(error) => {
                return LoadedRulesPolicy {
                    policy: RulesConfig::default(),
                    diagnostic: Some(RulesPolicyDiagnostic::Read {
                        failure: RulesPolicyReadFailure::InvalidUtf8,
                        detail: format!(
                            "invalid UTF-8 at byte {}",
                            error.utf8_error().valid_up_to()
                        ),
                    }),
                    present: true,
                };
            }
        };
        match serde_yaml::from_str::<ProjectConfig>(&raw) {
            Ok(config) => LoadedRulesPolicy {
                policy: config.rules,
                diagnostic: None,
                present: true,
            },
            Err(error) => LoadedRulesPolicy {
                policy: RulesConfig::default(),
                diagnostic: Some(RulesPolicyDiagnostic::Parse {
                    detail: error.to_string(),
                }),
                present: true,
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

    fn policy_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("leveler-rules-policy-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(".leveler")).unwrap();
        dir
    }

    #[test]
    fn an_absent_policy_file_is_not_a_diagnostic() {
        let dir = policy_dir("absent");
        let loaded = ProjectConfig::load_rules_policy(&dir);
        assert!(!loaded.present);
        assert!(loaded.diagnostic.is_none());
        assert!(loaded.policy.always_delivered.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_parse_error_is_a_parse_diagnostic_not_a_read_one() {
        let dir = policy_dir("parse");
        std::fs::write(dir.join(".leveler/config.yaml"), "rules: [unclosed\n").unwrap();
        let loaded = ProjectConfig::load_rules_policy(&dir);
        assert!(loaded.present);
        assert!(matches!(
            loaded.diagnostic,
            Some(RulesPolicyDiagnostic::Parse { .. })
        ));
        assert!(loaded.policy.always_delivered.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_utf8_policy_bytes_are_a_read_diagnostic() {
        let dir = policy_dir("invalid-utf8");
        std::fs::write(dir.join(".leveler/config.yaml"), [0xff, 0xfe]).unwrap();
        let loaded = ProjectConfig::load_rules_policy(&dir);
        assert!(loaded.present);
        assert!(matches!(
            loaded.diagnostic,
            Some(RulesPolicyDiagnostic::Read {
                failure: RulesPolicyReadFailure::InvalidUtf8,
                ..
            })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_in_place_of_the_policy_is_a_read_diagnostic() {
        let dir = policy_dir("directory");
        std::fs::create_dir_all(dir.join(".leveler/config.yaml")).unwrap();
        let loaded = ProjectConfig::load_rules_policy(&dir);
        assert!(loaded.present);
        assert!(matches!(
            loaded.diagnostic,
            Some(RulesPolicyDiagnostic::Read { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_policy_file_is_a_read_diagnostic() {
        use std::os::unix::fs::PermissionsExt;
        let dir = policy_dir("unreadable");
        let path = dir.join(".leveler/config.yaml");
        std::fs::write(&path, "rules:\n  budget_bytes: 10\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
            std::fs::remove_dir_all(&dir).ok();
            eprintln!("skipping: this process can read mode-000 files (running as root?)");
            return;
        }
        let loaded = ProjectConfig::load_rules_policy(&dir);
        assert!(loaded.present);
        assert!(matches!(
            loaded.diagnostic,
            Some(RulesPolicyDiagnostic::Read {
                failure: RulesPolicyReadFailure::PermissionDenied,
                ..
            })
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}
