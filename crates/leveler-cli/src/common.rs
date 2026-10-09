//! Shared CLI helpers: model resolution, mode mapping, approver construction,
//! and the Ctrl+C interrupt handler.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_app::Application;
use leveler_execution::{Approver, AutoApprove, PermissionProfile};
use leveler_model::ModelRef;
use leveler_project::Layout;

use crate::approver;
use crate::cli::RunMode;
use crate::output::Line;

/// Resolve the model reference: CLI flag, else `.leveler/config.yaml` default,
/// else the global default, else the first configured model.
///
/// A persisted default this environment cannot resolve is abandoned with a
/// notice rather than carried forward: carrying it used to end in
/// `model … is not configured` at the first model call, which for the TUI meant
/// the interface never opened at all.
pub(crate) fn resolve_model(app: &Application, model: Option<String>) -> anyhow::Result<ModelRef> {
    let (chosen, notice) = resolve_model_with_notice(app, model)?;
    if let Some(notice) = notice {
        // Every caller of this is a one-shot command whose stderr is the
        // person's screen; an interactive caller takes the notice instead.
        eprintln!("{notice}");
    }
    Ok(chosen)
}

/// [`resolve_model`], carrying the abandoned-default notice to the caller so an
/// interactive front end can show it where the person will actually see it.
pub(crate) fn resolve_model_with_notice(
    app: &Application,
    model: Option<String>,
) -> anyhow::Result<(ModelRef, Option<String>)> {
    let (chosen, notice) = choose_model(app, model)?;
    // Every caller of this is about to make a model call. A provider whose key
    // is missing is knowable here, and saying so here is the difference between
    // a fix the user can apply and the provider's own wire advice.
    ensure_provider_key(&app.config.providers, &chosen)?;
    Ok((chosen, notice))
}

fn choose_model(
    app: &Application,
    model: Option<String>,
) -> anyhow::Result<(ModelRef, Option<String>)> {
    // An explicit flag is the user's own intent: used verbatim, and a bad value
    // fails later with the resolver's own message rather than being silently
    // replaced by something else.
    if let Some(m) = model {
        return Ok((parse_model_ref(&m)?, None));
    }
    let configured = app.model_refs();
    let persisted = [
        app.project_config()
            .model
            .as_deref()
            .and_then(ModelRef::parse),
        app.config
            .default_model
            .as_deref()
            .and_then(ModelRef::parse),
    ];
    let mut abandoned: Vec<String> = Vec::new();
    for candidate in persisted.into_iter().flatten() {
        if configured.contains(&candidate) {
            let notice = notice_for(&abandoned, &candidate);
            return Ok((candidate, notice));
        }
        abandoned.push(candidate.to_string());
    }
    let mut refs = configured;
    refs.sort_by_key(|r| r.to_string());
    refs.into_iter()
        .next()
        .map(|chosen| {
            let notice = notice_for(&abandoned, &chosen);
            (chosen, notice)
        })
        .ok_or_else(|| {
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

/// The one wording for "a persisted default was not usable here", shared by
/// the headless stderr path and the interactive startup notice.
fn notice_for(abandoned: &[String], chosen: &ModelRef) -> Option<String> {
    if abandoned.is_empty() {
        return None;
    }
    Some(format!(
        "默认模型 {} 在本环境不可用（已从配置中移除？），本次改用 {chosen}；\
         用 /model 选择一个可用的模型",
        abandoned.join("、")
    ))
}

pub(crate) fn map_mode(mode: RunMode) -> PermissionProfile {
    match mode {
        RunMode::RequestApproval => PermissionProfile::RequestApproval,
        RunMode::Assisted => PermissionProfile::Assisted,
        RunMode::FullAccess => PermissionProfile::FullAccess,
    }
}

/// Resolve the permission mode a launch runs under.
///
/// Precedence, highest first:
///
/// 1. an explicit runtime `SetPermissionProfile` — already persisted, so it
///    reaches this resolution as the persisted/fallback value;
/// 2. an explicit CLI `--permission`;
/// 3. the session's persisted mode (the `fallback`);
/// 4. the project's configured default, resolved by the caller.
///
/// `None` means the flag was NOT supplied. It is never "the flag defaulted",
/// which is what used to let a default silently overwrite a persisted mode.
pub(crate) fn resolve_mode(
    explicit: Option<RunMode>,
    fallback: PermissionProfile,
) -> PermissionProfile {
    explicit.map(map_mode).unwrap_or(fallback)
}

/// The protocol mirror of [`PermissionProfile`]. One conversion, so a resolved
/// mode cannot be spelled two ways on the wire.
pub(crate) fn wire_mode(mode: PermissionProfile) -> leveler_client_protocol::PermissionProfile {
    match mode {
        PermissionProfile::RequestApproval => {
            leveler_client_protocol::PermissionProfile::RequestApproval
        }
        PermissionProfile::Assisted => leveler_client_protocol::PermissionProfile::Assisted,
        PermissionProfile::FullAccess => leveler_client_protocol::PermissionProfile::FullAccess,
    }
}

/// The project's configured default permission mode (`.leveler/config.yaml`
/// `mode:`), else the built-in `assisted`.
///
/// This is the fallback for a NEW session only. A resumed session uses its
/// persisted mode as the fallback instead, so a project default can never
/// overwrite a mode the user already chose for that session.
pub(crate) fn project_default_mode(layout: &Layout) -> PermissionProfile {
    layout
        .primary_workspace()
        .and_then(leveler_project::ProjectConfig::load)
        .and_then(|config| config.mode)
        .and_then(|raw| PermissionProfile::parse(&raw))
        .unwrap_or(PermissionProfile::Assisted)
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

#[cfg(test)]
mod mode_resolution_tests {
    use super::*;

    /// Precedence 1/2/3 at the resolution layer: an explicit flag always wins;
    /// absence keeps the fallback (a persisted mode on resume, a project
    /// default on create).
    #[test]
    fn an_explicit_flag_wins_and_absence_keeps_the_fallback() {
        assert_eq!(
            resolve_mode(None, PermissionProfile::FullAccess),
            PermissionProfile::FullAccess,
            "absence must keep the persisted mode"
        );
        assert_eq!(
            resolve_mode(Some(RunMode::Assisted), PermissionProfile::FullAccess),
            PermissionProfile::Assisted,
            "an explicit flag must override the persisted mode"
        );
        assert_eq!(
            resolve_mode(Some(RunMode::FullAccess), PermissionProfile::Assisted),
            PermissionProfile::FullAccess
        );
        assert_eq!(
            resolve_mode(Some(RunMode::RequestApproval), PermissionProfile::Assisted),
            PermissionProfile::RequestApproval
        );
    }

    /// Precedence 4: the project config is only a fallback, and only for a new
    /// session. A project default may not overwrite an explicit flag.
    #[test]
    fn the_project_default_is_only_a_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".leveler")).unwrap();
        std::fs::write(
            tmp.path().join(".leveler/config.yaml"),
            "mode: full_access\n",
        )
        .unwrap();
        let layout = Layout::from_parts(
            tmp.path().to_path_buf(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        );
        assert_eq!(project_default_mode(&layout), PermissionProfile::FullAccess);
        assert_eq!(
            resolve_mode(None, project_default_mode(&layout)),
            PermissionProfile::FullAccess
        );
        assert_eq!(
            resolve_mode(Some(RunMode::Assisted), project_default_mode(&layout)),
            PermissionProfile::Assisted
        );
    }

    /// No project config (or an unparseable mode) means the built-in default.
    #[test]
    fn an_absent_project_default_is_assisted() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::from_parts(
            tmp.path().to_path_buf(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        );
        assert_eq!(project_default_mode(&layout), PermissionProfile::Assisted);
    }
}
