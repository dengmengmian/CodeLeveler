//! Load/save the active theme id in the user-level Leveler config.
//!
//! Writes `[ui] theme = "…"` under `$LEVELER_HOME/config.toml` or
//! `~/.leveler/config.toml` with format-preserving `toml_edit` so unrelated
//! tables and comments survive.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, value};

use crate::theme::ThemeId;

/// Config path: `<leveler-home>/config.toml`, or `None` when no home is known.
/// Shares the home-resolution order via [`leveler_core::leveler_home_dir`].
pub fn config_path() -> Option<PathBuf> {
    leveler_core::leveler_home_dir(leveler_core::environment()).map(|home| home.join("config.toml"))
}

/// Read the theme id string from config, if present and valid.
pub fn load_theme_id() -> Option<ThemeId> {
    let path = config_path()?;
    load_theme_id_at(&path)
}

/// Read theme id from a specific config file (tests).
pub fn load_theme_id_at(path: &Path) -> Option<ThemeId> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_theme_id_from_toml(&text)
}

/// Parse `ui.theme` from TOML text.
pub fn parse_theme_id_from_toml(text: &str) -> Option<ThemeId> {
    let doc = text.parse::<DocumentMut>().ok()?;
    let raw = doc
        .get("ui")
        .and_then(|ui| ui.get("theme"))
        .and_then(|t| t.as_str())?;
    ThemeId::parse(raw)
}

/// Persist a theme id, creating parent dirs and the file as needed.
pub fn save_theme_id(id: ThemeId) -> Result<(), String> {
    let path =
        config_path().ok_or_else(|| "no config path (set HOME or LEVELER_HOME)".to_string())?;
    save_theme_id_at(&path, id)
}

/// Persist theme id to a specific path (tests).
pub fn save_theme_id_at(path: &Path, id: ThemeId) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse::<DocumentMut>()
            .map_err(|e| format!("config is not valid TOML: {e}"))?
    };
    if !doc.contains_key("ui") {
        doc["ui"] = Item::Table(toml_edit::Table::new());
    }
    doc["ui"]["theme"] = value(id.as_str());
    std::fs::write(path, doc.to_string()).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_theme_id_in_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_model = \"deepseek/x\"\n\n[providers.p]\nbase_url = \"http://x\"\n",
        )
        .unwrap();
        save_theme_id_at(&path, ThemeId::Night).unwrap();
        assert_eq!(load_theme_id_at(&path), Some(ThemeId::Night));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("default_model"),
            "must preserve other keys: {text}"
        );
        assert!(text.contains("[ui]"), "{text}");
        assert!(text.contains("theme"), "{text}");
        // Re-save day
        save_theme_id_at(&path, ThemeId::Day).unwrap();
        assert_eq!(load_theme_id_at(&path), Some(ThemeId::Day));
    }

    #[test]
    fn parse_theme_from_inline_toml() {
        assert_eq!(
            parse_theme_id_from_toml("[ui]\ntheme = \"ion\"\n"),
            Some(ThemeId::Ion)
        );
        assert_eq!(parse_theme_id_from_toml("lang = \"zh\"\n"), None);
    }
}
