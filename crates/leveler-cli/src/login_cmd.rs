//! `leveler login` / `leveler logout` — put an API key into the global config.
//!
//! Before this, a working install still required knowing what an environment
//! variable is and re-exporting it in every shell. `init` only ever asked for
//! the *name* of that variable. This asks for the key itself and stores it, so
//! a fresh terminal just works.
//!
//! The key goes into `~/.leveler/config.toml` as plaintext `api_key` — the
//! format already supports it, and `leveler config show` prints only
//! `api_key_env`, never the key. The file is tightened to `0600` on write:
//! once it holds a secret, its old world-readable default is wrong.

use std::io::Write;

use anyhow::Context;
use toml_edit::{DocumentMut, value};

use crate::output::Line;

/// Insert or replace `providers.<id>.api_key` in a config document.
///
/// Returns the rewritten TOML. Everything else — comments, ordering, other
/// providers, model tables — is preserved, because this edits a file people
/// hand-maintain.
pub(crate) fn upsert_api_key(
    config: &str,
    provider_id: &str,
    api_key: &str,
) -> anyhow::Result<String> {
    set_provider_field(config, provider_id, "api_key", api_key.trim())
}

/// Set one field on `providers.<id>`, preserving the rest of the document.
pub(crate) fn set_provider_field(
    config: &str,
    provider_id: &str,
    field: &str,
    field_value: &str,
) -> anyhow::Result<String> {
    let mut doc: DocumentMut = config.parse().context("global config is not valid TOML")?;
    let providers = doc["providers"].or_insert(toml_edit::table());
    if let Some(table) = providers.as_table_mut() {
        table.set_implicit(true);
    }
    let entry = providers[provider_id].or_insert(toml_edit::table());
    entry[field] = value(field_value);
    Ok(doc.to_string())
}

/// The wire-protocol name the global config parser expects.
///
/// `render_init_config` omits `protocol`, and the parser silently defaults a
/// missing value to `openai_chat` — so a preset on any other protocol has to
/// write it explicitly or it produces a config that talks the wrong wire format.
pub(crate) fn protocol_key(protocol: leveler_model::ProtocolKind) -> &'static str {
    use leveler_model::ProtocolKind;
    match protocol {
        ProtocolKind::OpenAiChat => "openai_chat",
        ProtocolKind::OpenAiResponses => "openai_responses",
        ProtocolKind::AnthropicMessages => "anthropic_messages",
        ProtocolKind::GeminiGenerateContent => "gemini_generate_content",
    }
}

/// Remove `providers.<id>.api_key`. Returns `None` when there was nothing to
/// remove, so the caller can say so instead of claiming a logout happened.
pub(crate) fn remove_api_key(config: &str, provider_id: &str) -> anyhow::Result<Option<String>> {
    let mut doc: DocumentMut = config.parse().context("global config is not valid TOML")?;
    let Some(providers) = doc.get_mut("providers").and_then(|p| p.as_table_mut()) else {
        return Ok(None);
    };
    let Some(entry) = providers
        .get_mut(provider_id)
        .and_then(|e| e.as_table_mut())
    else {
        return Ok(None);
    };
    if entry.remove("api_key").is_none() {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// Tighten the config to owner-only. It now holds a secret; the default mode
/// would leave it readable by every account on the machine.
#[cfg(unix)]
fn tighten(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn tighten(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

pub(crate) async fn cmd_login(provider: Option<String>) -> anyhow::Result<std::process::ExitCode> {
    let path = leveler_app::GlobalConfig::path()
        .context("cannot resolve a home directory for the global config")?;
    // No config yet: this is someone's first run. Build one from a preset
    // rather than sending them to `init` to answer questions (base URL,
    // protocol, context window) that nobody can answer before using the tool.
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return first_run_setup(&path, provider.as_deref()).await,
    };

    let configured = configured_providers(&existing);
    let provider_id = match provider {
        Some(id) => id,
        None => match configured.as_slice() {
            [] => {
                println!("{}", Line::warn("No providers configured."));
                println!("  Add one with `leveler init`, or edit {}", path.display());
                return Ok(std::process::ExitCode::from(1));
            }
            [only] => only.clone(),
            many => {
                println!("Configured providers: {}", many.join(", "));
                prompt_line(&format!("provider [{}]", many[0]))?
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| many[0].clone())
            }
        },
    };
    if !configured.iter().any(|p| p == &provider_id) {
        println!(
            "{}",
            Line::warn(&format!("Provider `{provider_id}` is not in the config."))
        );
        println!("  Known: {}", configured.join(", "));
        return Ok(std::process::ExitCode::from(1));
    }

    let key = read_secret(&format!("API key for {provider_id}"))?;
    if key.trim().is_empty() {
        println!("{}", Line::warn("Empty key — nothing written."));
        return Ok(std::process::ExitCode::from(1));
    }

    let updated = upsert_api_key(&existing, &provider_id, &key)?;
    std::fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
    tighten(&path).ok();

    println!("{}", Line::ok(&format!("Saved key for {provider_id}")));
    println!("  {} (owner-only)", path.display());
    println!("\nVerify with: leveler doctor");
    Ok(std::process::ExitCode::SUCCESS)
}

/// First-run setup: pick a known provider, name a model, paste a key.
///
/// Writes a complete, working config. The model is offered as an editable
/// default because the preset cannot know which models a given key can reach —
/// see `leveler_provider::presets`.
async fn first_run_setup(
    path: &std::path::Path,
    provider: Option<&str>,
) -> anyhow::Result<std::process::ExitCode> {
    use leveler_provider::presets::{PRESETS, preset};

    let chosen = match provider {
        Some(id) => match preset(id) {
            Some(p) => p,
            None => {
                println!("{}", Line::warn(&format!("Unknown provider `{id}`.")));
                println!("  Built-in: {}", preset_ids().join(", "));
                println!("  For anything else, run `leveler init` and edit the config.");
                return Ok(std::process::ExitCode::from(1));
            }
        },
        None => {
            println!("{}", Line::heading("leveler login"));
            println!("  No config yet — setting one up.\n");
            for (i, p) in PRESETS.iter().enumerate() {
                println!("    {}) {}", i + 1, p.label);
            }
            println!();
            let answer = prompt_line("provider [1]")?.unwrap_or_default();
            let index = if answer.is_empty() {
                0
            } else {
                match answer.parse::<usize>() {
                    Ok(n) if (1..=PRESETS.len()).contains(&n) => n - 1,
                    _ => match preset(answer.trim()) {
                        Some(p) => PRESETS.iter().position(|x| x.id == p.id).unwrap_or(0),
                        None => {
                            println!("{}", Line::warn("Not one of the listed choices."));
                            return Ok(std::process::ExitCode::from(1));
                        }
                    },
                }
            };
            &PRESETS[index]
        }
    };

    println!();
    println!("  {} · {}", chosen.label, chosen.base_url);
    println!("\n  Get a key at: {}", chosen.console_url);
    let key = read_secret(&format!("API key for {}", chosen.label))?;
    if key.trim().is_empty() {
        println!("{}", Line::warn("Empty key — nothing written."));
        return Ok(std::process::ExitCode::from(1));
    }

    // Ask the provider what this key can actually reach instead of making the
    // user invent a model id. The preset's suggestion is only the fallback for
    // gateways with no /models endpoint.
    let model = choose_model(chosen, &key).await?;

    let template = leveler_app::global_config::render_init_config(
        chosen.id,
        chosen.base_url,
        chosen.key_env,
        &model,
        chosen.suggested_context,
    );
    let with_proto = set_provider_field(
        &template,
        chosen.id,
        "protocol",
        protocol_key(chosen.protocol),
    )?;
    let with_key = upsert_api_key(&with_proto, chosen.id, &key)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, with_key).with_context(|| format!("write {}", path.display()))?;
    tighten(path).ok();

    println!();
    println!("{}", Line::ok(&format!("Ready — {}/{}", chosen.id, model)));
    println!("  {} (owner-only)", path.display());
    println!("\nTry it:");
    println!("  leveler            # interactive UI");
    println!("  leveler doctor     # verify the setup");
    Ok(std::process::ExitCode::SUCCESS)
}

/// Offer the models this key can actually reach; fall back to the preset's
/// suggestion when the provider has no `/models` endpoint.
async fn choose_model(
    preset: &leveler_provider::presets::ProviderPreset,
    api_key: &str,
) -> anyhow::Result<String> {
    let probe = leveler_provider::config::ProviderConfig {
        id: preset.id.to_string(),
        protocol: preset.protocol,
        base_url: preset.base_url.to_string(),
        api_key_env: String::new(),
        api_key: Some(api_key.to_string()),
        headers: Default::default(),
        timeouts: Default::default(),
        retry: Default::default(),
    };
    println!("\n  正在获取可用模型…");
    let available = match leveler_provider::discovery::list_remote_models(&probe).await {
        Ok(models) if !models.is_empty() => models,
        // Neither case is a failure: plenty of gateways do not implement it.
        Ok(_) => {
            println!("  (这个 key 没有列出可用模型)");
            Vec::new()
        }
        Err(e) => {
            println!("  ({e})");
            Vec::new()
        }
    };
    if available.is_empty() {
        return Ok(prompt_line(&format!("model [{}]", preset.suggested_model))?
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| preset.suggested_model.to_string()));
    }

    let shown: Vec<&String> = available.iter().take(20).collect();
    for (i, m) in shown.iter().enumerate() {
        println!("    {}) {m}", i + 1);
    }
    if available.len() > shown.len() {
        println!(
            "    … 另有 {} 个未列出（可直接输入名称）",
            available.len() - shown.len()
        );
    }
    let answer = prompt_line("model [1]")?.unwrap_or_default();
    if answer.is_empty() {
        return Ok(shown[0].to_string());
    }
    if let Ok(n) = answer.parse::<usize>()
        && (1..=shown.len()).contains(&n)
    {
        return Ok(shown[n - 1].to_string());
    }
    // A typed name is accepted verbatim — the listing may be truncated.
    Ok(answer)
}

fn preset_ids() -> Vec<&'static str> {
    leveler_provider::presets::PRESETS
        .iter()
        .map(|p| p.id)
        .collect()
}

/// Read a secret without echoing it — a pasted key otherwise stays in
/// scrollback and in any terminal recording.
fn read_secret(label: &str) -> anyhow::Result<String> {
    print!("  {label} (input hidden): ");
    std::io::stdout().flush().ok();
    let key = console::Term::stdout()
        .read_secure_line()
        .context("read API key")?;
    println!();
    Ok(key)
}

pub(crate) fn cmd_logout(provider: String) -> anyhow::Result<std::process::ExitCode> {
    let path = leveler_app::GlobalConfig::path()
        .context("cannot resolve a home directory for the global config")?;
    let existing =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    match remove_api_key(&existing, &provider)? {
        Some(updated) => {
            std::fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
            tighten(&path).ok();
            println!("{}", Line::ok(&format!("Removed key for {provider}")));
            Ok(std::process::ExitCode::SUCCESS)
        }
        None => {
            println!(
                "{}",
                Line::warn(&format!("No stored key for `{provider}` — nothing to do."))
            );
            Ok(std::process::ExitCode::from(1))
        }
    }
}

/// Provider ids declared in the config, in file order.
fn configured_providers(config: &str) -> Vec<String> {
    config
        .parse::<DocumentMut>()
        .ok()
        .and_then(|doc| {
            doc.get("providers")
                .and_then(|p| p.as_table())
                .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
        })
        .unwrap_or_default()
}

fn prompt_line(label: &str) -> anyhow::Result<Option<String>> {
    print!("  {label}: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read answer")?;
    Ok(Some(line.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"default_model = "deepseek/deepseek-chat"

# Keep my notes
[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[providers.kimi]
base_url = "https://api.kimi.com"
api_key_env = "KIMI_API_KEY"

[models.deepseek-chat]
provider = "deepseek"
context_window = 131072
"#;

    #[test]
    fn the_key_lands_under_the_right_provider() {
        let out = upsert_api_key(SAMPLE, "deepseek", "sk-abc").unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["providers"]["deepseek"]["api_key"].as_str(),
            Some("sk-abc")
        );
        assert!(
            doc["providers"]["kimi"].get("api_key").is_none(),
            "only the named provider may be touched"
        );
    }

    /// This file is hand-maintained; a login must not reformat it or drop the
    /// user's comments.
    #[test]
    fn everything_else_survives_the_edit() {
        let out = upsert_api_key(SAMPLE, "deepseek", "sk-abc").unwrap();
        assert!(out.contains("# Keep my notes"), "{out}");
        assert!(out.contains("api_key_env = \"DEEPSEEK_API_KEY\""));
        assert!(out.contains("context_window = 131072"));
        assert!(out.contains("[providers.kimi]"));
    }

    #[test]
    fn logging_in_twice_replaces_the_key() {
        let once = upsert_api_key(SAMPLE, "deepseek", "sk-old").unwrap();
        let twice = upsert_api_key(&once, "deepseek", "sk-new").unwrap();
        let doc: DocumentMut = twice.parse().unwrap();
        assert_eq!(
            doc["providers"]["deepseek"]["api_key"].as_str(),
            Some("sk-new")
        );
        assert_eq!(twice.matches("api_key =").count(), 1, "no duplicate key");
    }

    #[test]
    fn a_key_with_special_characters_round_trips() {
        let weird = r#"sk-"quoted"\and\backslash"#;
        let out = upsert_api_key(SAMPLE, "deepseek", weird).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["providers"]["deepseek"]["api_key"].as_str(),
            Some(weird)
        );
    }

    #[test]
    fn logout_removes_only_the_key() {
        let with_key = upsert_api_key(SAMPLE, "deepseek", "sk-abc").unwrap();
        let out = remove_api_key(&with_key, "deepseek")
            .unwrap()
            .expect("removed");
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc["providers"]["deepseek"].get("api_key").is_none());
        assert_eq!(
            doc["providers"]["deepseek"]["api_key_env"].as_str(),
            Some("DEEPSEEK_API_KEY"),
            "the env fallback must remain so the provider still works"
        );
    }

    /// Reporting a logout that removed nothing would tell the user their key is
    /// gone when it never was there.
    #[test]
    fn logging_out_without_a_stored_key_reports_nothing_removed() {
        assert!(remove_api_key(SAMPLE, "deepseek").unwrap().is_none());
        assert!(remove_api_key(SAMPLE, "nosuch").unwrap().is_none());
    }

    #[test]
    fn configured_providers_are_listed_in_file_order() {
        assert_eq!(configured_providers(SAMPLE), vec!["deepseek", "kimi"]);
    }

    /// `render_init_config` omits `protocol` and the parser defaults a missing
    /// value to openai_chat, so an Anthropic preset would silently produce a
    /// config that speaks the wrong wire format.
    #[test]
    fn every_preset_writes_a_config_that_names_its_protocol() {
        use leveler_provider::presets::PRESETS;
        for p in PRESETS {
            let template = leveler_app::global_config::render_init_config(
                p.id,
                p.base_url,
                p.key_env,
                p.suggested_model,
                p.suggested_context,
            );
            let out = set_provider_field(&template, p.id, "protocol", protocol_key(p.protocol))
                .and_then(|t| upsert_api_key(&t, p.id, "sk-test"))
                .unwrap();
            let doc: DocumentMut = out.parse().unwrap();
            assert_eq!(
                doc["providers"][p.id]["protocol"].as_str(),
                Some(protocol_key(p.protocol)),
                "{} must declare its protocol",
                p.id
            );
            assert_eq!(
                doc["providers"][p.id]["base_url"].as_str(),
                Some(p.base_url)
            );
            assert_eq!(doc["providers"][p.id]["api_key"].as_str(), Some("sk-test"));
            assert_eq!(
                doc["default_model"].as_str(),
                Some(format!("{}/{}", p.id, p.suggested_model).as_str()),
                "{} must be selectable straight away",
                p.id
            );
        }
    }

    #[test]
    fn protocol_names_match_what_the_parser_accepts() {
        use leveler_model::ProtocolKind;
        assert_eq!(protocol_key(ProtocolKind::OpenAiChat), "openai_chat");
        assert_eq!(
            protocol_key(ProtocolKind::AnthropicMessages),
            "anthropic_messages"
        );
    }

    #[test]
    fn a_broken_config_is_refused_rather_than_overwritten() {
        assert!(upsert_api_key("not toml {{{", "deepseek", "sk").is_err());
    }
}
