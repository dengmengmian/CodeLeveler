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

use std::io::{IsTerminal, Write};

use anyhow::Context;
use toml_edit::{DocumentMut, value};

use crate::output::Line;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRunAction {
    Continue,
    Guide,
    RefuseNonInteractive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterFirstRun {
    ReturnToShell,
    StartTui,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRunLanguage {
    En,
    Zh,
}

impl FirstRunLanguage {
    fn pick(self, en: &'static str, zh: &'static str) -> &'static str {
        match self {
            Self::En => en,
            Self::Zh => zh,
        }
    }

    fn code(self) -> &'static str {
        self.pick("en", "zh")
    }
}

fn parse_first_run_language(answer: &str) -> Option<FirstRunLanguage> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "en" | "english" => Some(FirstRunLanguage::En),
        "2" | "zh" | "chinese" | "中文" => Some(FirstRunLanguage::Zh),
        _ => None,
    }
}

fn first_run_action(config_exists: bool, interactive: bool) -> FirstRunAction {
    if config_exists {
        FirstRunAction::Continue
    } else if interactive {
        FirstRunAction::Guide
    } else {
        FirstRunAction::RefuseNonInteractive
    }
}

fn choose_first_run_language() -> anyhow::Result<FirstRunLanguage> {
    println!("Choose your language:");
    println!("    1) English");
    println!("    2) 中文");
    loop {
        let answer = prompt_line("language [1]")?.unwrap_or_default();
        if let Some(language) = parse_first_run_language(&answer) {
            return Ok(language);
        }
        println!("  Please choose 1 for English or 2 for Chinese.");
    }
}

/// Reuse `leveler login`'s first-run flow before opening the TUI.
///
/// `None` means the caller can continue into the TUI. `Some(code)` means setup
/// was cancelled or cannot run safely and the caller should return that code.
pub(crate) async fn ensure_first_run_config_for_tui()
-> anyhow::Result<Option<std::process::ExitCode>> {
    let path = leveler_app::GlobalConfig::path()
        .context("cannot resolve a home directory for the global config")?;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    match first_run_action(path.exists(), interactive) {
        FirstRunAction::Continue => Ok(None),
        FirstRunAction::RefuseNonInteractive => {
            eprintln!("No CodeLeveler config found / 未找到 CodeLeveler 配置。");
            eprintln!("  Run `leveler login` in a terminal to configure a provider and model.");
            eprintln!("  请在终端运行 `leveler login` 配置供应商和模型。");
            Ok(Some(std::process::ExitCode::FAILURE))
        }
        FirstRunAction::Guide => {
            let code = first_run_setup(&path, None, AfterFirstRun::StartTui).await?;
            if code != std::process::ExitCode::SUCCESS {
                Ok(Some(code))
            } else {
                Ok(None)
            }
        }
    }
}

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
    let table = entry
        .as_table_mut()
        .context("provider config entry must be a table")?;
    table[field] = value(field_value);
    let comment = match field {
        "api_key" => Some((
            "本机保存的 API Key，请勿提交到版本库。",
            "Locally stored API key; never commit this file.",
        )),
        "protocol" => Some((
            "供应商使用的接口协议。",
            "API protocol used by this provider.",
        )),
        _ => None,
    };
    if let Some((zh, en)) = comment
        && let Some(mut key) = table.key_mut(field)
    {
        key.leaf_decor_mut().set_prefix(format!("# {zh}\n# {en}\n"));
    }
    Ok(doc.to_string())
}

fn set_config_language(config: &str, language: FirstRunLanguage) -> anyhow::Result<String> {
    let mut doc: DocumentMut = config.parse().context("global config is not valid TOML")?;
    doc["lang"] = value(language.code());
    if let Some(mut key) = doc.as_table_mut().key_mut("lang") {
        key.leaf_decor_mut().set_prefix(
            "# 界面语言；环境变量 LEVELER_LANG 的优先级更高。\n# UI language; LEVELER_LANG takes precedence.\n",
        );
    }
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
        Err(_) => {
            return first_run_setup(&path, provider.as_deref(), AfterFirstRun::ReturnToShell).await;
        }
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
    after: AfterFirstRun,
) -> anyhow::Result<std::process::ExitCode> {
    use leveler_provider::presets::{PRESETS, preset};

    // This is deliberately the first user-facing output. No locale exists yet,
    // so the language question itself is always in English.
    let language = choose_first_run_language()?;

    let chosen = match provider {
        Some(id) => match preset(id) {
            Some(p) => p,
            None => {
                println!(
                    "{}",
                    Line::warn(&format!(
                        "{} `{id}`.",
                        language.pick("Unknown provider", "未知供应商")
                    ))
                );
                println!(
                    "  {}: {}",
                    language.pick("Built-in", "内置供应商"),
                    preset_ids().join(", ")
                );
                println!(
                    "  {}",
                    language.pick(
                        "For anything else, run `leveler init` and edit the config.",
                        "其他供应商请运行 `leveler init`，然后编辑配置文件。",
                    )
                );
                return Ok(std::process::ExitCode::from(1));
            }
        },
        None => {
            println!("{}", Line::heading("leveler login"));
            println!(
                "  {}\n",
                language.pick(
                    "No config yet — setting one up.",
                    "尚未找到配置，现在开始设置。"
                )
            );
            for (i, p) in PRESETS.iter().enumerate() {
                println!("    {}) {}", i + 1, provider_label(p, language));
            }
            println!();
            let answer =
                prompt_line(language.pick("provider [1]", "供应商 [1]"))?.unwrap_or_default();
            let index = if answer.is_empty() {
                0
            } else {
                match answer.parse::<usize>() {
                    Ok(n) if (1..=PRESETS.len()).contains(&n) => n - 1,
                    _ => match preset(answer.trim()) {
                        Some(p) => PRESETS.iter().position(|x| x.id == p.id).unwrap_or(0),
                        None => {
                            println!(
                                "{}",
                                Line::warn(language.pick(
                                    "Not one of the listed choices.",
                                    "输入不在可选列表中。"
                                ))
                            );
                            return Ok(std::process::ExitCode::from(1));
                        }
                    },
                }
            };
            &PRESETS[index]
        }
    };

    println!();
    let chosen_label = provider_label(chosen, language);
    println!("  {chosen_label} · {}", chosen.base_url);
    println!(
        "\n  {}: {}",
        language.pick("Get an API key at", "获取 API Key"),
        chosen.console_url
    );
    let key = read_first_run_secret(chosen_label, language)?;
    if key.trim().is_empty() {
        println!(
            "{}",
            Line::warn(language.pick(
                "Empty key — nothing written.",
                "API Key 为空，未写入任何配置。"
            ))
        );
        return Ok(std::process::ExitCode::from(1));
    }

    // Ask the provider what this key can actually reach instead of making the
    // user invent a model id. The preset's suggestion is only the fallback for
    // gateways with no /models endpoint.
    let model = choose_model(chosen, &key, language).await?;

    let with_proto = starter_config(chosen, &model)?;
    let with_language = set_config_language(&with_proto, language)?;
    let with_key = upsert_api_key(&with_language, chosen.id, &key)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, with_key).with_context(|| format!("write {}", path.display()))?;
    tighten(path).ok();

    println!();
    println!(
        "{}",
        Line::ok(&format!(
            "{} — {}/{}",
            language.pick("Ready", "配置完成"),
            chosen.id,
            model
        ))
    );
    println!(
        "  {} ({})",
        path.display(),
        language.pick("owner-only", "仅当前用户可读写")
    );
    match after {
        AfterFirstRun::ReturnToShell => {
            println!("\n{}:", language.pick("Try it", "接下来可以运行"));
            println!(
                "  leveler            # {}",
                language.pick("interactive UI", "打开交互界面")
            );
            println!(
                "  leveler doctor     # {}",
                language.pick("verify the setup", "检查配置")
            );
        }
        AfterFirstRun::StartTui => {
            println!(
                "  {}",
                language.pick(
                    "Run `leveler doctor` later to verify the setup.",
                    "稍后可运行 `leveler doctor` 检查配置。"
                )
            );
            println!(
                "\n{}\n",
                language.pick("Starting CodeLeveler…", "正在启动 CodeLeveler…")
            );
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn starter_config(
    preset: &leveler_provider::presets::ProviderPreset,
    model_id: &str,
) -> anyhow::Result<String> {
    let template = match leveler_provider::builtin_model_profile(preset.id, model_id)? {
        Some(profile) => leveler_app::global_config::render_init_config_with_profile(
            preset.id,
            preset.base_url,
            preset.key_env,
            &profile,
        ),
        None => leveler_app::global_config::render_init_config(
            preset.id,
            preset.base_url,
            preset.key_env,
            model_id,
            preset.suggested_context,
        ),
    };
    set_provider_field(
        &template,
        preset.id,
        "protocol",
        protocol_key(preset.protocol),
    )
}

/// Offer the models this key can actually reach; fall back to the preset's
/// suggestion when the provider has no `/models` endpoint.
async fn choose_model(
    preset: &leveler_provider::presets::ProviderPreset,
    api_key: &str,
    language: FirstRunLanguage,
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
    println!(
        "\n  {}",
        language.pick("Fetching available models…", "正在获取可用模型…")
    );
    let available = match leveler_provider::discovery::list_remote_models(&probe).await {
        Ok(models) if !models.is_empty() => models,
        // Neither case is a failure: plenty of gateways do not implement it.
        Ok(_) => {
            println!(
                "  ({})",
                language.pick(
                    "this key did not list any available models",
                    "这个 Key 没有列出可用模型"
                )
            );
            Vec::new()
        }
        Err(e) => {
            println!("  ({e})");
            Vec::new()
        }
    };
    if available.is_empty() {
        return Ok(prompt_line(&format!(
            "{} [{}]",
            language.pick("model", "模型"),
            preset.suggested_model
        ))?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| preset.suggested_model.to_string()));
    }

    let shown: Vec<&String> = available.iter().take(20).collect();
    for (i, m) in shown.iter().enumerate() {
        println!("    {}) {m}", i + 1);
    }
    if available.len() > shown.len() {
        let more = match language {
            FirstRunLanguage::En => format!(
                "{} more not shown (type a name directly)",
                available.len() - shown.len()
            ),
            FirstRunLanguage::Zh => format!(
                "另有 {} 个未列出（可直接输入名称）",
                available.len() - shown.len()
            ),
        };
        println!("    … {more}");
    }
    let answer = prompt_line(language.pick("model [1]", "模型 [1]"))?.unwrap_or_default();
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

fn provider_label(
    preset: &leveler_provider::presets::ProviderPreset,
    language: FirstRunLanguage,
) -> &'static str {
    if preset.id == "bigmodel" {
        language.pick("Zhipu BigModel", "智谱 BigModel")
    } else {
        preset.label
    }
}

fn read_first_run_secret(provider: &str, language: FirstRunLanguage) -> anyhow::Result<String> {
    let label = match language {
        FirstRunLanguage::En => format!("API key for {provider}"),
        FirstRunLanguage::Zh => format!("{provider} API Key"),
    };
    print!(
        "  {label} ({}): ",
        language.pick("input hidden", "输入已隐藏")
    );
    std::io::stdout().flush().ok();
    let key = console::Term::stdout()
        .read_secure_line()
        .context("read API key")?;
    println!();
    Ok(key)
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
    fn first_tui_launch_guides_only_an_interactive_unconfigured_user() {
        assert_eq!(first_run_action(true, true), FirstRunAction::Continue);
        assert_eq!(first_run_action(true, false), FirstRunAction::Continue);
        assert_eq!(first_run_action(false, true), FirstRunAction::Guide);
        assert_eq!(
            first_run_action(false, false),
            FirstRunAction::RefuseNonInteractive
        );
    }

    #[test]
    fn language_is_the_first_explicit_first_run_choice() {
        assert_eq!(parse_first_run_language(""), Some(FirstRunLanguage::En));
        assert_eq!(parse_first_run_language("1"), Some(FirstRunLanguage::En));
        assert_eq!(
            parse_first_run_language("english"),
            Some(FirstRunLanguage::En)
        );
        assert_eq!(parse_first_run_language("2"), Some(FirstRunLanguage::Zh));
        assert_eq!(parse_first_run_language("中文"), Some(FirstRunLanguage::Zh));
        assert_eq!(parse_first_run_language("3"), None);
    }

    #[test]
    fn selected_language_is_persisted_with_bilingual_help() {
        for (language, expected) in [(FirstRunLanguage::En, "en"), (FirstRunLanguage::Zh, "zh")] {
            let text = set_config_language(SAMPLE, language).unwrap();
            let doc: DocumentMut = text.parse().unwrap();
            assert_eq!(doc["lang"].as_str(), Some(expected));
            assert!(text.contains("界面语言"), "{text}");
            assert!(text.contains("UI language"), "{text}");
        }
    }

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
        assert!(out.contains("本机保存的 API Key"), "{out}");
        assert!(out.contains("Locally stored API key"), "{out}");
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
            assert!(out.contains("供应商使用的接口协议"), "{out}");
            assert!(out.contains("API protocol used by this provider"), "{out}");
            assert!(out.contains("本机保存的 API Key"), "{out}");
            assert!(out.contains("Locally stored API key"), "{out}");
            assert_eq!(
                doc["default_model"].as_str(),
                Some(format!("{}/{}", p.id, p.suggested_model).as_str()),
                "{} must be selectable straight away",
                p.id
            );
        }
    }

    #[test]
    fn bigmodel_login_writes_complete_glm_model_facts() {
        use leveler_provider::presets::preset;

        for (model, vision) in [("glm-5.3", false), ("glm-5.3-flash", true)] {
            let text = starter_config(preset("bigmodel").unwrap(), model).unwrap();
            let doc: DocumentMut = text.parse().unwrap();
            let facts = &doc["models"][model];
            assert_eq!(facts["model_id"].as_str(), Some(model));
            assert_eq!(facts["vision"].as_bool(), Some(vision));
            assert_eq!(facts["reasoning"].as_bool(), Some(true));
            assert_eq!(facts["reasoning_style"].as_str(), Some("thinking_flag"));
            let efforts = facts["supported_efforts"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>();
            assert_eq!(efforts, vec!["low", "high", "max"]);
            assert_eq!(facts["reasoning_effort"].as_str(), Some("max"));
            assert_eq!(facts["context_window"].as_integer(), Some(1_048_576));
            assert_eq!(facts["reliable_context"].as_integer(), Some(786_432));
            assert_eq!(facts["max_output_tokens"].as_integer(), Some(131_072));
        }
    }

    #[test]
    fn deepseek_login_writes_complete_builtin_model_facts() {
        use leveler_provider::presets::preset;

        for (model, vision) in [("deepseek-flash", true), ("deepseek-v4-pro", false)] {
            let text = starter_config(preset("deepseek").unwrap(), model).unwrap();
            let doc: DocumentMut = text.parse().unwrap();
            let facts = &doc["models"][model];
            assert_eq!(facts["model_id"].as_str(), Some(model));
            assert_eq!(facts["vision"].as_bool(), Some(vision));
            assert_eq!(facts["reasoning"].as_bool(), Some(true));
            assert_eq!(facts["parallel_tool_calls"].as_bool(), Some(true));
            assert_eq!(facts["context_window"].as_integer(), Some(1_048_576));
            assert_eq!(facts["max_output_tokens"].as_integer(), Some(393_216));
            assert_eq!(facts["supports_temperature"].as_bool(), Some(true));
            assert_eq!(facts["max_parallel_tool_calls"].as_integer(), Some(0));
            assert_eq!(facts["passback_reasoning_content"].as_bool(), Some(true));
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
