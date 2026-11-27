//! The Agent browser default (`[browser].default`): which product the browser
//! capability drives when a call does not name one.
//!
//! This is not the Desktop's own manual browser pane. It is read once when the
//! runtime assembles its browser handle, so a saved value is honest about its
//! scope: the change reaches the next runtime start, never the live handle.

use std::path::Path;

pub use leveler_browser::BrowserProduct;
use serde::Serialize;

use crate::global_config::GlobalConfig;

#[derive(Debug, thiserror::Error)]
pub enum BrowserSettingsError {
    #[error("config is not valid TOML: {0}")]
    Parse(String),
    #[error("{0}")]
    Rejected(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("the configuration kept changing while it was being updated; nothing was written")]
    ConcurrentModification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserOptionView {
    pub id: String,
    pub label: String,
    /// Whether this machine can drive the product right now. Safari's answer
    /// only establishes macOS + safaridriver; the system setting is checked at
    /// launch, so it is never reported as a proven capability.
    pub available: bool,
    /// A caveat worth showing, never a path or an internal error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserSettingsView {
    pub configured: Option<String>,
    /// What the runtime would pick now: the configured product, else the
    /// automation default (Chrome → Edge → Chromium; Safari is opt-in).
    pub effective: Option<String>,
    pub options: Vec<BrowserOptionView>,
    /// The runtime assembles its browser handle once; a save needs a restart.
    pub restart_required: Vec<String>,
}

pub fn view(
    path: &Path,
    env: &leveler_core::EnvSnapshot,
) -> Result<BrowserSettingsView, BrowserSettingsError> {
    let text = read_text(path)?;
    let config = GlobalConfig::from_toml_str(&text)
        .map_err(|error| BrowserSettingsError::Parse(error.to_string()))?;
    let configured_product = config.into_bundle().browser_default;
    let configured = configured_product.map(|product| product.as_str().to_string());
    let effective = configured_product
        .or_else(|| leveler_browser::automation_default_product(env))
        .map(|product| product.as_str().to_string());
    let options = BrowserProduct::ALL
        .iter()
        .map(|product| BrowserOptionView {
            id: product.as_str().to_string(),
            label: product_label(*product).to_string(),
            available: leveler_browser::availability(env, *product).is_ok(),
            note: matches!(product, BrowserProduct::Safari).then(|| {
                "需要在 Safari 设置中开启“允许远程自动化”；桌面无法读取该设置".to_string()
            }),
        })
        .collect();
    Ok(BrowserSettingsView {
        configured,
        effective,
        options,
        restart_required: vec!["browser".to_string()],
    })
}

pub fn save(path: &Path, product: Option<BrowserProduct>) -> Result<(), BrowserSettingsError> {
    crate::config_edit::edit(
        path,
        |doc| match product {
            Some(product) => {
                if !doc.contains_key("browser") {
                    doc["browser"] = toml_edit::Item::Table(toml_edit::Table::new());
                }
                let table = doc["browser"].as_table_mut().ok_or_else(|| {
                    BrowserSettingsError::Rejected("`browser` 不是 TOML 表".to_string())
                })?;
                table["default"] = toml_edit::value(product.as_str());
                Ok(())
            }
            None => {
                let remove_table = match doc.get_mut("browser") {
                    None => false,
                    Some(item) => match item.as_table_mut() {
                        Some(table) => {
                            table.remove("default");
                            table.is_empty()
                        }
                        None => {
                            return Err(BrowserSettingsError::Rejected(
                                "`browser` 不是 TOML 表".to_string(),
                            ));
                        }
                    },
                };
                if remove_table {
                    doc.remove("browser");
                }
                Ok(())
            }
        },
        |candidate| {
            GlobalConfig::from_toml_str(candidate)
                .map(|_| ())
                .map_err(|error| BrowserSettingsError::Rejected(error.to_string()))
        },
    )
    .map_err(|error| match error {
        crate::config_edit::EditError::Apply(error)
        | crate::config_edit::EditError::Validate(error) => error,
        crate::config_edit::EditError::Parse(message) => BrowserSettingsError::Parse(message),
        crate::config_edit::EditError::Io(message) => BrowserSettingsError::Io(message),
        crate::config_edit::EditError::ConcurrentModification => {
            BrowserSettingsError::ConcurrentModification
        }
    })
}

fn product_label(product: BrowserProduct) -> &'static str {
    match product {
        BrowserProduct::Safari => "Safari",
        BrowserProduct::Chrome => "Google Chrome",
        BrowserProduct::Edge => "Microsoft Edge",
        BrowserProduct::Chromium => "Chromium",
    }
}

fn read_text(path: &Path) -> Result<String, BrowserSettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(BrowserSettingsError::Io(format!(
            "{}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_is_written_canonically_and_auto_removes_only_its_own_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "default_model = \"p/m\"\n").unwrap();
        save(&path, Some(BrowserProduct::Chrome)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[browser]") && text.contains("default = \"chrome\""),
            "{text}"
        );
        assert!(text.contains("default_model"), "unrelated keys survive");

        save(&path, None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("[browser]"), "{text}");
        assert!(text.contains("default_model"));
    }

    #[test]
    fn an_unknown_product_is_rejected_by_the_loader() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[browser]\ndefault = \"firefox\"\n").unwrap();
        assert!(view(&path, leveler_core::environment()).is_err());
    }
}
