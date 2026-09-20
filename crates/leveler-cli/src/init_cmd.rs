//! `leveler init` — create `~/.leveler/config.toml` interactively.
//!
//! Deliberately explicit: startup never writes config on its own (a silent
//! `$HOME` write from an agent CLI is exactly what security-conscious users
//! audit for). Data directories are created on demand; configuration is
//! created only here, with the user answering the questions.

use std::io::{IsTerminal, Write};

use anyhow::Context;

use crate::output::Line;

/// Defaults offered at each prompt (empty answer accepts them).
const DEFAULT_PROVIDER: &str = "deepseek";
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-flash";
const DEFAULT_CONTEXT_WINDOW: u64 = 1_048_576;

pub(crate) fn cmd_init() -> anyhow::Result<std::process::ExitCode> {
    let path = leveler_app::GlobalConfig::path()
        .context("cannot resolve a home directory for the global config")?;

    if path.exists() {
        println!(
            "{}",
            Line::warn(&format!("Global config already exists: {}", path.display()))
        );
        println!("  Edit it directly, or inspect it with: leveler config show");
        println!("  (init never overwrites an existing config.)");
        return Ok(std::process::ExitCode::from(1));
    }

    // Non-interactive: print the default template to stdout and touch nothing,
    // so `leveler init > somewhere` and scripted setups stay predictable.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        print!(
            "{}",
            render_config(
                DEFAULT_PROVIDER,
                DEFAULT_BASE_URL,
                &default_key_env(DEFAULT_PROVIDER),
                DEFAULT_MODEL,
                DEFAULT_CONTEXT_WINDOW,
            )?
        );
        eprintln!("# non-interactive: template printed, nothing written.");
        eprintln!("# save it to: {}", path.display());
        return Ok(std::process::ExitCode::SUCCESS);
    }

    println!("{}", Line::heading("leveler init"));
    println!("  Creates {} (empty answer = default)\n", path.display());

    let provider = prompt("provider id", DEFAULT_PROVIDER)?;
    let base_url = prompt("base URL", DEFAULT_BASE_URL)?;
    let key_env = prompt("API key env var", &default_key_env(&provider))?;
    let model = prompt("model id", DEFAULT_MODEL)?;
    let window_raw = prompt(
        "context window (tokens)",
        &DEFAULT_CONTEXT_WINDOW.to_string(),
    )?;
    let context_window: u64 = window_raw
        .replace('_', "")
        .parse()
        .with_context(|| format!("context window must be a number, got `{window_raw}`"))?;

    let text = render_config(&provider, &base_url, &key_env, &model, context_window)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;

    println!();
    println!("{}", Line::ok(&format!("Wrote {}", path.display())));
    println!("\nNext steps:");
    println!("  export {key_env}=…        # your API key");
    println!("  leveler doctor            # verify the setup");
    println!("  leveler run \"…\"           # or `leveler tui`");
    Ok(std::process::ExitCode::SUCCESS)
}

fn render_config(
    provider: &str,
    base_url: &str,
    key_env: &str,
    model: &str,
    context_window: u64,
) -> anyhow::Result<String> {
    Ok(
        match leveler_provider::builtin_model_profile(provider, model)? {
            Some(profile) if u64::from(profile.limits.context_window) == context_window => {
                leveler_app::global_config::render_init_config_with_profile(
                    provider, base_url, key_env, &profile,
                )
            }
            _ => leveler_app::global_config::render_init_config(
                provider,
                base_url,
                key_env,
                model,
                context_window,
            ),
        },
    )
}

/// `PROVIDER_API_KEY`, uppercased, non-alphanumerics folded to `_`.
fn default_key_env(provider: &str) -> String {
    let mut base: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if base.is_empty() {
        base.push_str("PROVIDER");
    }
    format!("{base}_API_KEY")
}

fn prompt(label: &str, default: &str) -> anyhow::Result<String> {
    print!("  {label} [{default}]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read answer")?;
    let answer = line.trim();
    Ok(if answer.is_empty() {
        default.to_string()
    } else {
        answer.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BASE_URL, DEFAULT_CONTEXT_WINDOW, DEFAULT_MODEL, default_key_env, render_config,
    };
    use toml_edit::DocumentMut;

    #[test]
    fn key_env_is_derived_from_the_provider_id() {
        assert_eq!(default_key_env("deepseek"), "DEEPSEEK_API_KEY");
        assert_eq!(default_key_env("my-provider"), "MY_PROVIDER_API_KEY");
        assert_eq!(default_key_env(""), "PROVIDER_API_KEY");
    }

    #[test]
    fn default_deepseek_template_uses_the_complete_flash_profile() {
        let text = render_config(
            "deepseek",
            DEFAULT_BASE_URL,
            "DEEPSEEK_API_KEY",
            DEFAULT_MODEL,
            DEFAULT_CONTEXT_WINDOW,
        )
        .unwrap();
        let doc: DocumentMut = text.parse().unwrap();
        let profile = &doc["models"]["deepseek-flash"];
        assert_eq!(profile["model_id"].as_str(), Some("deepseek-flash"));
        assert_eq!(profile["context_window"].as_integer(), Some(1_048_576));
        assert_eq!(profile["max_output_tokens"].as_integer(), Some(393_216));
        assert_eq!(profile["reasoning"].as_bool(), Some(true));
        assert_eq!(profile["vision"].as_bool(), Some(true));
        assert_eq!(profile["parallel_tool_calls"].as_bool(), Some(true));
        assert_eq!(profile["supports_temperature"].as_bool(), Some(true));
        assert_eq!(profile["max_parallel_tool_calls"].as_integer(), Some(0));
        assert_eq!(profile["passback_reasoning_content"].as_bool(), Some(true));
    }

    #[test]
    fn explicit_nondefault_context_window_is_preserved() {
        let text = render_config(
            "deepseek",
            DEFAULT_BASE_URL,
            "DEEPSEEK_API_KEY",
            DEFAULT_MODEL,
            262_144,
        )
        .unwrap();
        let doc: DocumentMut = text.parse().unwrap();
        assert_eq!(
            doc["models"]["deepseek-flash"]["context_window"].as_integer(),
            Some(262_144)
        );
    }
}
