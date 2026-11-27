//! Desktop toggles for live global options that are not provider or search
//! facts: multi-agent delegation, Git attribution and the two idle assists.
//!
//! These keys are read by the running runtime and by the Git commit adapter,
//! so a save goes through the same locked config transaction as every other
//! editor and is validated before it replaces the file.

use std::path::Path;

use serde::Serialize;

use crate::global_config::GlobalConfig;

#[derive(Debug, thiserror::Error)]
pub enum AgentOptionsError {
    #[error("config is not valid TOML: {0}")]
    Parse(String),
    #[error("{0}")]
    Rejected(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("the configuration kept changing while it was being updated; nothing was written")]
    ConcurrentModification,
}

/// The resolved values the Desktop renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AgentOptionsView {
    pub delegation: bool,
    pub co_author: bool,
    pub prompt_suggestions: bool,
    pub away_summary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentOptionsDraft {
    pub delegation: bool,
    pub co_author: bool,
    pub prompt_suggestions: bool,
    pub away_summary: bool,
}

pub fn view(path: &Path) -> Result<AgentOptionsView, AgentOptionsError> {
    let text = read_text(path)?;
    let bundle = GlobalConfig::from_toml_str(&text)
        .map_err(|error| AgentOptionsError::Parse(error.to_string()))?
        .into_bundle();
    Ok(AgentOptionsView {
        delegation: bundle.agents_delegation,
        co_author: bundle.vcs_co_author,
        prompt_suggestions: bundle.assist_prompt_suggestions,
        away_summary: bundle.assist_away_summary,
    })
}

pub fn save(path: &Path, draft: &AgentOptionsDraft) -> Result<(), AgentOptionsError> {
    crate::config_edit::edit(
        path,
        |doc| {
            for (table, key, value) in [
                ("agents", "delegation", draft.delegation),
                ("vcs", "co_author", draft.co_author),
                ("assist", "prompt_suggestions", draft.prompt_suggestions),
                ("assist", "away_summary", draft.away_summary),
            ] {
                if !doc.contains_key(table) {
                    doc[table] = toml_edit::Item::Table(toml_edit::Table::new());
                }
                let table = doc[table].as_table_mut().ok_or_else(|| {
                    AgentOptionsError::Rejected(format!("`{table}` 不是 TOML 表"))
                })?;
                table[key] = toml_edit::value(value);
            }
            Ok(())
        },
        |candidate| {
            GlobalConfig::from_toml_str(candidate)
                .map(|_| ())
                .map_err(|error| AgentOptionsError::Rejected(error.to_string()))
        },
    )
    .map_err(|error| match error {
        crate::config_edit::EditError::Apply(error)
        | crate::config_edit::EditError::Validate(error) => error,
        crate::config_edit::EditError::Parse(message) => AgentOptionsError::Parse(message),
        crate::config_edit::EditError::Io(message) => AgentOptionsError::Io(message),
        crate::config_edit::EditError::ConcurrentModification => {
            AgentOptionsError::ConcurrentModification
        }
    })
}

fn read_text(path: &Path) -> Result<String, AgentOptionsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(AgentOptionsError::Io(format!(
            "{}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_and_each_key_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "default_model = \"p/m\"\n").unwrap();
        assert_eq!(
            view(&path).unwrap(),
            AgentOptionsView {
                delegation: true,
                co_author: true,
                prompt_suggestions: true,
                away_summary: true
            }
        );
        save(
            &path,
            &AgentOptionsDraft {
                delegation: false,
                co_author: false,
                prompt_suggestions: false,
                away_summary: false,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[agents]") && text.contains("delegation = false"),
            "{text}"
        );
        assert!(
            text.contains("[vcs]") && text.contains("co_author = false"),
            "{text}"
        );
        assert!(text.contains("[assist]"), "{text}");
        assert_eq!(
            view(&path).unwrap(),
            AgentOptionsView {
                delegation: false,
                co_author: false,
                prompt_suggestions: false,
                away_summary: false
            }
        );
        assert!(text.contains("default_model"), "unrelated keys survive");
    }
}
