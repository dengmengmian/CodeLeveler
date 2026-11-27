//! The global Thinking default, read and written through the one locked
//! config transaction.
//!
//! This is a root key (`thinking = "high"`), not a table: the loader rejects a
//! `[thinking]` table. The session's `/thinking` override and a model's
//! `[models.<id>].thinking` still outrank it; this editor only moves the
//! default those two fall back to.

use std::path::Path;

use leveler_model::ThinkingLevel;
use serde::Serialize;

use crate::global_config::GlobalConfig;

#[derive(Debug, thiserror::Error)]
pub enum ThinkingSettingsError {
    #[error("config is not valid TOML: {0}")]
    Parse(String),
    #[error("{0}")]
    Rejected(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("the configuration kept changing while it was being updated; nothing was written")]
    ConcurrentModification,
}

/// What a client renders: the value written in the file (if any), the value a
/// new session actually gets, and the only levels a person may choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThinkingSettingsView {
    /// `None` means no explicit default is stored; `effective` is still real.
    pub configured: Option<String>,
    pub effective: String,
    pub choices: Vec<String>,
}

pub fn view(path: &Path) -> Result<ThinkingSettingsView, ThinkingSettingsError> {
    let text = read_text(path)?;
    let config = GlobalConfig::from_toml_str(&text)
        .map_err(|error| ThinkingSettingsError::Parse(error.to_string()))?;
    Ok(ThinkingSettingsView {
        configured: config.thinking.map(|level| level.as_str().to_string()),
        effective: config.thinking_level().as_str().to_string(),
        choices: ThinkingLevel::ALL
            .iter()
            .map(|level| level.as_str().to_string())
            .collect(),
    })
}

/// Store `Some(level)` or remove the key (`None` restores the product
/// default). Validation is the same full-config parse every writer uses.
pub fn save(path: &Path, level: Option<ThinkingLevel>) -> Result<(), ThinkingSettingsError> {
    crate::config_edit::edit(
        path,
        |doc| {
            match level {
                Some(level) => doc["thinking"] = toml_edit::value(level.as_str()),
                None => {
                    doc.remove("thinking");
                }
            }
            Ok(())
        },
        |candidate| {
            GlobalConfig::from_toml_str(candidate)
                .map(|_| ())
                .map_err(|error| ThinkingSettingsError::Rejected(error.to_string()))
        },
    )
    .map_err(|error| match error {
        crate::config_edit::EditError::Apply(error)
        | crate::config_edit::EditError::Validate(error) => error,
        crate::config_edit::EditError::Parse(message) => ThinkingSettingsError::Parse(message),
        crate::config_edit::EditError::Io(message) => ThinkingSettingsError::Io(message),
        crate::config_edit::EditError::ConcurrentModification => {
            ThinkingSettingsError::ConcurrentModification
        }
    })
}

fn read_text(path: &Path) -> Result<String, ThinkingSettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(ThinkingSettingsError::Io(format!(
            "{}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_key_is_high_and_an_explicit_level_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "default_model = \"p/m\"\n").unwrap();
        let initial = view(&path).unwrap();
        assert_eq!(initial.configured, None);
        assert_eq!(initial.effective, "high");
        assert!(initial.choices.contains(&"auto".to_string()));

        save(&path, Some(ThinkingLevel::Low)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("thinking = \"low\""), "{text}");
        let saved = view(&path).unwrap();
        assert_eq!(saved.configured.as_deref(), Some("low"));
        assert_eq!(saved.effective, "low");
        assert!(text.contains("default_model"), "unrelated keys survive");

        save(&path, None).unwrap();
        assert_eq!(view(&path).unwrap().configured, None);
    }

    /// Thinking and the browser default share ONE config transaction: two
    /// concurrent editors must both land, and neither may drop the other's key
    /// or the provider credential already in the file.
    #[test]
    fn thinking_and_browser_saves_share_the_lock_and_preserve_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "default_model = \"p/m\"\n[providers.p]\nprotocol = \"openai_chat\"\nbase_url = \"https://example.test\"\napi_key = \"sk-keep-me\"\n",
        )
        .unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| save(&path, Some(ThinkingLevel::Low)).unwrap());
            scope.spawn(|| {
                crate::browser_config::save(
                    &path,
                    Some(crate::browser_config::BrowserProduct::Chrome),
                )
                .unwrap();
            });
        });
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("thinking = \"low\""), "{text}");
        assert!(
            text.contains("[browser]") && text.contains("default = \"chrome\""),
            "{text}"
        );
        assert!(
            text.contains("sk-keep-me"),
            "the credential survives: {text}"
        );
        assert_eq!(view(&path).unwrap().configured.as_deref(), Some("low"));
    }
}
