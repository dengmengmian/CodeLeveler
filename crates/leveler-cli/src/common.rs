//! Shared CLI helpers: model resolution, mode mapping, approver construction,
//! and the Ctrl+C interrupt handler.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_app::Application;
use leveler_execution::{Approver, AutoApprove, PermissionProfile};
use leveler_model::ModelRef;

use crate::approver;
use crate::cli::RunMode;
use crate::output::Line;

/// Resolve the model reference: CLI flag, else `.leveler/config.yaml` default,
/// else the single configured model.
pub(crate) fn resolve_model(app: &Application, model: Option<String>) -> anyhow::Result<ModelRef> {
    let chosen = choose_model(app, model)?;
    // Every caller of this is about to make a model call. A provider whose key
    // is missing is knowable here, and saying so here is the difference between
    // a fix the user can apply and the provider's own wire advice.
    ensure_provider_key(&app.config.providers, &chosen)?;
    Ok(chosen)
}

fn choose_model(app: &Application, model: Option<String>) -> anyhow::Result<ModelRef> {
    if let Some(m) = model {
        return parse_model_ref(&m);
    }
    if let Some(m) = app
        .project_config()
        .model
        .as_deref()
        .and_then(ModelRef::parse)
    {
        return Ok(m);
    }
    // Global config default (~/.leveler/config.toml).
    if let Some(m) = app
        .config
        .default_model
        .as_deref()
        .and_then(ModelRef::parse)
    {
        return Ok(m);
    }
    let mut refs = app.model_refs();
    refs.sort_by_key(|r| r.to_string());
    refs.into_iter().next().ok_or_else(|| {
        anyhow::anyhow!(
            "no models configured\n\
                 \n\
                 Run `leveler init` to set this up interactively, or create \
                 ~/.leveler/config.toml (or $LEVELER_HOME/config.toml) with a \
                 provider and model, then set the API key env var. Example:\n\
                 \n\
                   default_model = \"deepseek/deepseek-chat\"\n\
                   [providers.deepseek]\n\
                   base_url = \"https://api.deepseek.com\"\n\
                   api_key_env = \"DEEPSEEK_API_KEY\"\n\
                   [models.\"deepseek-chat\"]\n\
                   provider = \"deepseek\"\n\
                   context_window = 131072\n\
                 \n\
                 Then: export DEEPSEEK_API_KEY=… && leveler doctor\n\
                 (Repo-local configs/models/*.yaml still works for developers.)"
        )
    })
}

pub(crate) fn map_mode(mode: RunMode) -> PermissionProfile {
    match mode {
        RunMode::RequestApproval => PermissionProfile::RequestApproval,
        RunMode::Assisted => PermissionProfile::Assisted,
        RunMode::FullAccess => PermissionProfile::FullAccess,
    }
}

pub(crate) fn build_approver(auto_approve: bool) -> Arc<dyn Approver> {
    if auto_approve {
        Arc::new(AutoApprove)
    } else {
        Arc::new(approver::CliApprover)
    }
}

/// Install a Ctrl+C handler that cancels the run gracefully (once).
pub(crate) fn spawn_interrupt_handler(token: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!(
                "\n{}",
                Line::warn("Interrupt received — cancelling current step…")
            );
            token.cancel();
        }
    });
}

pub(crate) fn parse_model_ref(model: &str) -> anyhow::Result<ModelRef> {
    ModelRef::parse(model).ok_or_else(|| {
        anyhow::anyhow!("invalid model reference `{model}` (expected `provider/model`)")
    })
}

/// Refuse a run whose provider has no usable API key, before any request.
///
/// The runtime already knows this locally: `resolve_api_key` reports a declared
/// `api_key_env` that is unset. Sending anyway earns the provider's own reply —
/// "You didn't provide an API key … in an Authorization header" — which names
/// no variable a user can set and no command they can run. `leveler doctor`
/// says it properly; a run should not say it worse.
///
/// `Ok(())` for a keyless provider (local models legitimately have none) and
/// for one whose key resolves.
pub(crate) fn ensure_provider_key(
    providers: &[leveler_provider::ProviderConfig],
    model_ref: &ModelRef,
) -> anyhow::Result<()> {
    let Some(cfg) = providers.iter().find(|p| p.id == model_ref.provider) else {
        // An unconfigured provider is a different failure with its own message.
        return Ok(());
    };
    match leveler_provider::resolve_api_key(cfg) {
        Ok(_) => Ok(()),
        Err(leveler_provider::ConfigError::MissingEnv(var)) => Err(anyhow::anyhow!(
            "provider `{}` has no API key: ${var} is not set\n\
             \n\
             Set it for this shell, or store one:\n\
             \n\
             \x20   export {var}=<your key>\n\
             \x20   leveler login {}\n\
             \n\
             `leveler doctor` lists every provider and which key each one wants.",
            cfg.id,
            cfg.id
        )),
        Err(other) => Err(anyhow::anyhow!(
            "provider `{}` has no usable API key: {other}\n\
             \n\
             Run `leveler doctor` to see which key it wants.",
            cfg.id
        )),
    }
}

#[cfg(test)]
mod key_preflight_tests {
    use super::*;
    use leveler_provider::ProviderConfig;

    fn provider(id: &str, key_env: &str, key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            base_url: "https://example.test/v1".into(),
            api_key_env: key_env.into(),
            api_key: key.map(str::to_string),
            protocol: leveler_model::ProtocolKind::OpenAiChat,
            headers: Default::default(),
            timeouts: Default::default(),
            retry: Default::default(),
        }
    }

    #[test]
    fn a_missing_key_names_the_variable_and_the_command_that_sets_it() {
        // A name nothing exports; the preflight reads the real environment.
        let var = "LEVELER_TEST_KEY_THAT_IS_NOT_SET";
        assert!(
            std::env::var(var).is_err(),
            "fixture assumes {var} is unset"
        );
        let providers = vec![provider("deepseek", var, None)];
        let err = ensure_provider_key(&providers, &ModelRef::new("deepseek", "v4"))
            .expect_err("a keyless provider must refuse before the request");
        let msg = err.to_string();
        assert!(msg.contains(var), "name the variable: {msg}");
        assert!(
            msg.contains("leveler login deepseek"),
            "name the fix: {msg}"
        );
        assert!(msg.contains("leveler doctor"), "point at doctor: {msg}");
        assert!(
            !msg.contains("Authorization header"),
            "the provider's wire advice is not an action a user can take: {msg}"
        );
    }

    #[test]
    fn a_configured_key_passes_and_a_keyless_provider_is_allowed() {
        let with_key = vec![provider("deepseek", "UNUSED", Some("sk-local"))];
        assert!(ensure_provider_key(&with_key, &ModelRef::new("deepseek", "v4")).is_ok());
        let keyless = vec![provider("ollama", "", None)];
        assert!(
            ensure_provider_key(&keyless, &ModelRef::new("ollama", "llama")).is_ok(),
            "a local model legitimately has no key"
        );
    }

    #[test]
    fn an_unconfigured_provider_is_left_to_its_own_error() {
        assert!(ensure_provider_key(&[], &ModelRef::new("nope", "m")).is_ok());
    }
}
