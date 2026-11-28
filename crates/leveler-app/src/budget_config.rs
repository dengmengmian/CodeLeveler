//! The budget settings editor behind the Desktop Settings page.
//!
//! One authority: the same `[limits]` merge the runtime resolves for a run.
//! The view reports the global file's values, the project file's explicit
//! overrides, and the effective value per dimension, so the page can say which
//! source wins instead of presenting one merged number as if it were the only
//! fact.
//!
//! Only the USER-GLOBAL file is edited here. A project's `.leveler/config.yaml`
//! is read-only for the Desktop: writing YAML would rewrite a file that may
//! carry comments and unrelated keys. The page shows the override and where it
//! lives; the user edits that file directly when they mean to change it.
//!
//! Caps are read at run construction, so a save governs the next turn, not a
//! running one. The page reports that contract instead of implying an effect
//! that has not happened.

use std::path::Path;

use leveler_client_protocol::DEFAULT_SOFT_LIMIT_PERCENT;
use leveler_project::{ProjectConfig, RunLimitsConfig};
use serde::Serialize;

use crate::global_config::{GlobalConfig, GlobalLimits};

/// One limits block as the page presents it. Absent means "not set here",
/// never zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BudgetValues {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_model_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost_usd_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_duration_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soft_limit_percent: Option<u8>,
}

impl BudgetValues {
    fn is_empty(&self) -> bool {
        self.max_model_tokens.is_none()
            && self.max_cost_usd_micros.is_none()
            && self.max_duration_seconds.is_none()
            && self.soft_limit_percent.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetSettingsView {
    /// Values in the user-global `config.toml` `[limits]` table.
    pub global: BudgetValues,
    /// Explicit values in the current project's `.leveler/config.yaml`.
    pub project: BudgetValues,
    /// Per dimension: the project value when set, else the global value.
    pub effective: BudgetValues,
    /// Why the global file could not be read as configuration, if it could
    /// not. The page shows this instead of pretending the caps are absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_error: Option<String>,
    /// The project file the overrides come from, when the workspace has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_config_path: Option<String>,
    /// True when the project file exists (even if this view could not parse it).
    pub project_present: bool,
    /// True when the project file exists but is not parseable; the runtime
    /// would fall back to defaults, so the page must not claim the overrides.
    pub project_invalid: bool,
    pub default_soft_limit_percent: u8,
    /// Caps are read when a run is built; a save is in force from the next
    /// turn. This is a contract statement, not a guess about a running turn.
    pub applies_next_run: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BudgetConfigError {
    #[error("预算数值无效：{0}")]
    Invalid(String),
    #[error("无法读取配置：{0}")]
    Parse(String),
    #[error("配置写入失败：{0}")]
    Io(String),
    #[error("配置在保存期间被其他进程修改，本次未写入")]
    Concurrent,
}

/// The page's read model. `config_path` is the global file
/// (`$LEVELER_HOME/config.toml` or `~/.leveler/config.toml`); `project_root`
/// is the current workspace, when one is selected.
pub fn view(config_path: &Path, project_root: Option<&Path>) -> BudgetSettingsView {
    let mut global = BudgetValues::default();
    let mut global_error = None;
    match std::fs::read_to_string(config_path) {
        Ok(text) => match GlobalConfig::from_toml_str(&text) {
            Ok(config) => global = values_from_global(config.limits()),
            Err(error) => global_error = Some(error.to_string()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => global_error = Some(format!("{}: {error}", config_path.display())),
    }

    let mut project = BudgetValues::default();
    let mut project_config_path = None;
    let mut project_present = false;
    let mut project_invalid = false;
    if let Some(root) = project_root {
        let path = root.join(".leveler/config.yaml");
        if path.exists() {
            project_config_path = Some(path.display().to_string());
            project_present = true;
            match std::fs::read_to_string(&path) {
                Ok(raw) => match ProjectConfig::parse(&raw) {
                    Some(config) => project = values_from_project(&config.limits),
                    None => project_invalid = true,
                },
                Err(_) => project_invalid = true,
            }
        }
    }

    let effective = BudgetValues {
        max_model_tokens: project.max_model_tokens.or(global.max_model_tokens),
        max_cost_usd_micros: project.max_cost_usd_micros.or(global.max_cost_usd_micros),
        max_duration_seconds: project.max_duration_seconds.or(global.max_duration_seconds),
        soft_limit_percent: project.soft_limit_percent.or(global.soft_limit_percent),
    };

    BudgetSettingsView {
        global,
        project,
        effective,
        global_error,
        project_config_path,
        project_present,
        project_invalid,
        default_soft_limit_percent: DEFAULT_SOFT_LIMIT_PERCENT,
        applies_next_run: true,
    }
}

/// Replace the user-global `[limits]` table. `None` removes a key; a draft
/// with every field absent removes the table, restoring "no configured cap"
/// rather than writing explicit zeros.
pub fn save(config_path: &Path, draft: &BudgetValues) -> Result<(), BudgetConfigError> {
    validate(draft)?;
    let result = crate::config_edit::edit(
        config_path,
        |doc| {
            if draft.is_empty() {
                doc.remove("limits");
                return Ok(());
            }
            if !doc.contains_key("limits") {
                doc["limits"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let table = doc["limits"]
                .as_table_mut()
                .ok_or_else(|| BudgetConfigError::Parse("`limits` 不是 TOML 表".to_string()))?;
            let number = |value: u64| toml_edit::value(i64::try_from(value).unwrap_or(i64::MAX));
            for (key, value) in [
                ("max_model_tokens", draft.max_model_tokens.map(number)),
                ("max_cost_usd_micros", draft.max_cost_usd_micros.map(number)),
                (
                    "max_duration_seconds",
                    draft.max_duration_seconds.map(number),
                ),
                (
                    "soft_limit_percent",
                    draft
                        .soft_limit_percent
                        .map(|value| number(u64::from(value))),
                ),
            ] {
                match value {
                    Some(value) => table[key] = value,
                    None => {
                        table.remove(key);
                    }
                }
            }
            Ok(())
        },
        |candidate| {
            GlobalConfig::from_toml_str(candidate)
                .map(|_| ())
                .map_err(|error| BudgetConfigError::Parse(error.to_string()))
        },
    );
    match result {
        Ok(()) => Ok(()),
        Err(
            crate::config_edit::EditError::Apply(error)
            | crate::config_edit::EditError::Validate(error),
        ) => Err(error),
        Err(crate::config_edit::EditError::Parse(message)) => {
            Err(BudgetConfigError::Parse(message))
        }
        Err(crate::config_edit::EditError::Io(message)) => Err(BudgetConfigError::Io(message)),
        Err(crate::config_edit::EditError::ConcurrentModification) => {
            Err(BudgetConfigError::Concurrent)
        }
    }
}

fn validate(draft: &BudgetValues) -> Result<(), BudgetConfigError> {
    const MAX_TOKENS: u64 = 1_000_000_000_000;
    const MAX_COST_MICROS: u64 = 1_000_000_000_000;
    const MAX_DURATION_SECONDS: u64 = 31_536_000;
    if draft
        .max_model_tokens
        .is_some_and(|value| value > MAX_TOKENS)
    {
        return Err(BudgetConfigError::Invalid(format!(
            "token 上限不能超过 {MAX_TOKENS}"
        )));
    }
    if draft
        .max_cost_usd_micros
        .is_some_and(|value| value > MAX_COST_MICROS)
    {
        return Err(BudgetConfigError::Invalid(format!(
            "费用上限不能超过 {MAX_COST_MICROS} 微美元（约 100 万美元）"
        )));
    }
    if draft
        .max_duration_seconds
        .is_some_and(|value| value > MAX_DURATION_SECONDS)
    {
        return Err(BudgetConfigError::Invalid(format!(
            "时长上限不能超过 {MAX_DURATION_SECONDS} 秒（一年）"
        )));
    }
    if draft
        .soft_limit_percent
        .is_some_and(|value| !(1..=100).contains(&value))
    {
        return Err(BudgetConfigError::Invalid(
            "提醒阈值必须在 1 到 100 之间".to_string(),
        ));
    }
    Ok(())
}

fn values_from_global(limits: GlobalLimits) -> BudgetValues {
    BudgetValues {
        max_model_tokens: limits.max_model_tokens,
        max_cost_usd_micros: limits.max_cost_usd_micros,
        max_duration_seconds: limits.max_duration_seconds,
        soft_limit_percent: limits.soft_limit_percent,
    }
}

fn values_from_project(limits: &RunLimitsConfig) -> BudgetValues {
    BudgetValues {
        max_model_tokens: limits.max_model_tokens,
        max_cost_usd_micros: limits.max_cost_usd_micros,
        max_duration_seconds: limits.max_duration_seconds,
        soft_limit_percent: limits.soft_limit_percent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_missing_file_reads_as_no_configured_cap() {
        let dir = tempfile::tempdir().unwrap();
        let view = view(&dir.path().join("config.toml"), None);
        assert!(view.global.is_empty());
        assert!(view.global_error.is_none());
        assert!(!view.project_present);
    }

    #[test]
    fn save_round_trips_every_supported_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "default_model = \"mock/m\"\n");
        let draft = BudgetValues {
            max_model_tokens: Some(50_000),
            max_cost_usd_micros: Some(250_000),
            max_duration_seconds: Some(600),
            soft_limit_percent: Some(75),
        };
        save(&path, &draft).unwrap();
        let read = view(&path, None);
        assert_eq!(read.global, draft);
        assert_eq!(read.effective, draft);
        assert!(read.global_error.is_none());
        // Unrelated keys survive the edit.
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("default_model")
        );
    }

    #[test]
    fn clearing_every_field_removes_the_table_instead_of_writing_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(
            &path,
            "default_model = \"mock/m\"\n\n[limits]\nmax_model_tokens = 5\n",
        );
        save(&path, &BudgetValues::default()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[limits]"));
        assert!(view(&path, None).global.is_empty());
    }

    #[test]
    fn project_overrides_win_per_dimension() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        write(
            &config,
            "[limits]\nmax_model_tokens = 100\nmax_duration_seconds = 60\n",
        );
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join(".leveler")).unwrap();
        write(
            &root.join(".leveler/config.yaml"),
            "limits:\n  max_model_tokens: 5\n",
        );
        let read = view(&config, Some(&root));
        assert_eq!(read.effective.max_model_tokens, Some(5));
        assert_eq!(read.effective.max_duration_seconds, Some(60));
        assert!(read.project_config_path.is_some());
    }

    #[test]
    fn an_invalid_project_file_is_reported_not_presented_as_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join(".leveler")).unwrap();
        write(&root.join(".leveler/config.yaml"), "limits: [not a map]\n");
        let read = view(&config, Some(&root));
        assert!(read.project_invalid);
        assert!(read.project.is_empty());
    }

    #[test]
    fn an_invalid_global_file_is_reported_instead_of_silently_uncapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "limits = [\n");
        let read = view(&path, None);
        assert!(read.global_error.is_some());
    }

    #[test]
    fn invalid_values_are_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "default_model = \"mock/m\"\n");
        let before = std::fs::read_to_string(&path).unwrap();
        let draft = BudgetValues {
            soft_limit_percent: Some(0),
            ..BudgetValues::default()
        };
        assert!(matches!(
            save(&path, &draft),
            Err(BudgetConfigError::Invalid(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
