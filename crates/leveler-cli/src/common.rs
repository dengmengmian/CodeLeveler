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
    ensure_provider_key(&app.effective_config().providers, &chosen)?;
    Ok((chosen, notice))
}

/// [`resolve_model_with_notice`], for a runtime that can also host
/// deterministic work.
///
/// A daemon is a runtime, not an agent invocation: `!command` and scheduled
/// shell actions are real host executions that never call a model. With no
/// model configured the runtime starts and this returns `None`; the first
/// agent turn reports the missing model from the session, where the person can
/// act on it. An explicit `--model` is still the user's intent and a bad value
/// still fails here.
pub(crate) fn resolve_optional_model_with_notice(
    app: &Application,
    model: Option<String>,
) -> anyhow::Result<(Option<ModelRef>, Option<String>)> {
    if model.is_none() && app.model_refs().is_empty() {
        return Ok((None, None));
    }
    let (chosen, notice) = resolve_model_with_notice(app, model)?;
    Ok((Some(chosen), notice))
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
    let effective = app.effective_config();
    let persisted = [
        app.project_config()
            .model
            .as_deref()
            .and_then(ModelRef::parse),
        effective.default_model.as_deref().and_then(ModelRef::parse),
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

/// The runtime profile for a profile a snapshot carried.
pub(crate) fn protocol_mode_to_runtime(
    mode: leveler_client_protocol::PermissionProfile,
) -> PermissionProfile {
    match mode {
        leveler_client_protocol::PermissionProfile::RequestApproval => {
            PermissionProfile::RequestApproval
        }
        leveler_client_protocol::PermissionProfile::Assisted => PermissionProfile::Assisted,
        leveler_client_protocol::PermissionProfile::FullAccess => PermissionProfile::FullAccess,
    }
}

/// The protocol mirror of [`leveler_execution::PermissionContract`].
pub(crate) fn wire_contract(
    contract: leveler_execution::PermissionContract,
) -> leveler_client_protocol::PermissionContract {
    match contract {
        leveler_execution::PermissionContract::V1 => {
            leveler_client_protocol::PermissionContract::V1
        }
        leveler_execution::PermissionContract::V2 => {
            leveler_client_protocol::PermissionContract::V2
        }
    }
}

/// Parse `--permission-contract`. Only V2 can be adopted; V1 has no visible
/// entry point, so the CLI never creates one from a flag.
pub(crate) fn parse_contract_flag(
    raw: Option<&str>,
) -> anyhow::Result<Option<leveler_execution::PermissionContract>> {
    raw.map(|value| {
        leveler_execution::PermissionContract::parse(value)
            .filter(|contract| *contract == leveler_execution::PermissionContract::V2)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "`{value}` is not an adoptable permission contract; only `v2` is accepted"
                )
            })
    })
    .transpose()
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

/// The envelope for an explicit `--permission` on an existing session.
///
/// `set_permission_profile` is a snapshot-versioned setting: the runtime
/// rejects a bare `send`, because a setting that replaces a user choice must
/// carry the version the user observed. `expected_version` comes from the
/// session snapshot the CLI just read; 0 covers a session with no events yet.
pub(crate) fn permission_change_envelope(
    session_id: &leveler_core::SessionId,
    mode: PermissionProfile,
    contract: Option<leveler_client_protocol::PermissionContract>,
    expected_version: i64,
) -> leveler_client_protocol::CommandEnvelope {
    leveler_client_protocol::CommandEnvelope {
        command_id: leveler_client_protocol::CommandId::generate(),
        session_id: session_id.clone(),
        expected_version: Some(expected_version),
        issued_at: leveler_core::now().to_rfc3339(),
        command: leveler_client_protocol::ClientCommand::SetPermissionProfile {
            session_id: session_id.clone(),
            mode: wire_mode(mode),
            contract,
        },
    }
}

/// Deliver an explicit permission change through the versioned setting path.
///
/// The CLI's `--permission` on an existing session is a user setting, not a
/// raw dispatch: routing it through `send` is exactly the D1 defect (the
/// runtime answers "snapshot version required" and the flag is lost).
pub(crate) async fn deliver_permission_change(
    client: &dyn leveler_client_protocol::InteractiveRuntimeClient,
    session_id: &leveler_core::SessionId,
    mode: PermissionProfile,
    contract: Option<leveler_client_protocol::PermissionContract>,
    expected_version: i64,
) -> Result<(), leveler_client_protocol::ClientError> {
    client
        .deliver(permission_change_envelope(
            session_id,
            mode,
            contract,
            expected_version,
        ))
        .await
}

/// Parse a configured mode name and say whether it is a pre-V2 name.
fn parse_mode_name(raw: &str) -> Option<(PermissionProfile, bool)> {
    match raw.trim() {
        "plan" => Some((PermissionProfile::RequestApproval, true)),
        "workspace_write" => Some((PermissionProfile::Assisted, true)),
        value => PermissionProfile::parse(value).map(|mode| (mode, false)),
    }
}

/// The project's configured `(mode, contract)` default.
///
/// - No config, or no `mode:`: the built-in default is a NEW session, so V2.
/// - A config that predates V2 (no `permission_contract`, or a legacy mode
///   name): V1. A repository that has not opted in must not silently acquire
///   the new Auto.
/// - `permission_contract: v2` with a current mode name: V2.
/// - An untrusted repository asking for Full falls back to the built-in
///   default (Assisted, V2); it may not widen authority.
pub(crate) fn project_permission_default(
    layout: &Layout,
) -> (PermissionProfile, leveler_execution::PermissionContract) {
    use leveler_execution::PermissionContract;
    let Some(root) = layout.primary_workspace() else {
        return (PermissionProfile::Assisted, PermissionContract::V2);
    };
    let trusted = leveler_execution::read_trusted_project_file(
        layout.home().root(),
        root,
        leveler_execution::PROJECT_CONFIG_RELATIVE,
    );
    let configured = match &trusted {
        leveler_execution::TrustedRead::Trusted(raw) => leveler_project::ProjectConfig::parse(raw),
        leveler_execution::TrustedRead::Untrusted { .. }
        | leveler_execution::TrustedRead::Absent => leveler_project::ProjectConfig::load(root),
    };
    let Some(config) = configured else {
        return (PermissionProfile::Assisted, PermissionContract::V2);
    };
    let Some((mode, legacy_name)) = config.mode.as_deref().and_then(parse_mode_name) else {
        return (PermissionProfile::Assisted, PermissionContract::V2);
    };
    if mode == PermissionProfile::FullAccess
        && !matches!(trusted, leveler_execution::TrustedRead::Trusted(_))
    {
        // An untrusted repository may ask for a stricter default, never a
        // wider one. The explicit `--permission full-access` flag still works:
        // it is the user's own decision and wins over this fallback.
        return (PermissionProfile::Assisted, PermissionContract::V2);
    }
    if legacy_name {
        return (mode, PermissionContract::V1);
    }
    let contract = config
        .permission_contract
        .as_deref()
        .and_then(PermissionContract::parse)
        .unwrap_or(PermissionContract::V1);
    (mode, contract)
}

/// The project's configured default permission mode. Kept as the mode-only
/// view for callers that do not create a session; new-session paths must use
/// [`project_permission_default`] so the contract travels with the tier.
pub(crate) fn project_default_mode(layout: &Layout) -> PermissionProfile {
    project_permission_default(layout).0
}

/// Resolve a contract: an explicit CLI adoption wins; otherwise the fallback
/// (the session's stored contract, or the project default).
pub(crate) fn resolve_contract(
    explicit: Option<leveler_execution::PermissionContract>,
    fallback: leveler_execution::PermissionContract,
) -> leveler_execution::PermissionContract {
    explicit.unwrap_or(fallback)
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
    /// session. A project default may not overwrite an explicit flag, and an
    /// untrusted repository may not raise the default above `assisted`.
    #[test]
    fn an_untrusted_project_cannot_raise_its_default_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".leveler")).unwrap();
        std::fs::write(repo.join(".leveler/config.yaml"), "mode: full_access\n").unwrap();
        let layout = Layout::from_parts(
            repo.clone(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        );
        assert_eq!(project_default_mode(&layout), PermissionProfile::Assisted);
        assert_eq!(
            resolve_mode(None, project_default_mode(&layout)),
            PermissionProfile::Assisted
        );
        assert_eq!(
            resolve_mode(Some(RunMode::Assisted), project_default_mode(&layout)),
            PermissionProfile::Assisted
        );
    }

    /// A repository may tighten its own default without trust: that direction
    /// never widens authority.
    #[test]
    fn an_untrusted_project_may_tighten_its_default_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".leveler")).unwrap();
        std::fs::write(
            repo.join(".leveler/config.yaml"),
            "mode: request_approval\n",
        )
        .unwrap();
        let layout = Layout::from_parts(repo, tmp.path().join("configs"), tmp.path().join("state"));
        assert_eq!(
            project_default_mode(&layout),
            PermissionProfile::RequestApproval
        );
    }

    /// Trusting the exact project config makes its widening mode the fallback.
    #[test]
    fn a_trusted_project_may_raise_its_default_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".leveler")).unwrap();
        let body = "mode: full_access\n";
        std::fs::write(repo.join(".leveler/config.yaml"), body).unwrap();
        let layout = Layout::from_parts(
            repo.clone(),
            tmp.path().join("configs"),
            tmp.path().join("state"),
        );
        let mut store = leveler_execution::TrustStore::load(layout.home().root());
        store.trust(
            &repo,
            leveler_execution::PROJECT_CONFIG_RELATIVE,
            body.as_bytes(),
        );
        store.save().unwrap();
        assert_eq!(project_default_mode(&layout), PermissionProfile::FullAccess);
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

    struct DeliveryOnly {
        delivered: std::sync::Mutex<Vec<leveler_client_protocol::CommandEnvelope>>,
    }

    #[async_trait::async_trait]
    impl leveler_client_protocol::InteractiveRuntimeClient for DeliveryOnly {
        async fn send(
            &self,
            _command: leveler_client_protocol::ClientCommand,
        ) -> Result<(), leveler_client_protocol::ClientError> {
            Err(leveler_client_protocol::ClientError::Runtime(
                "a versioned setting must not use the raw send path".into(),
            ))
        }

        async fn deliver(
            &self,
            envelope: leveler_client_protocol::CommandEnvelope,
        ) -> Result<(), leveler_client_protocol::ClientError> {
            self.delivered.lock().unwrap().push(envelope);
            Ok(())
        }

        fn subscribe(
            &self,
        ) -> tokio::sync::broadcast::Receiver<leveler_client_protocol::RuntimeEvent> {
            let (_, receiver) = tokio::sync::broadcast::channel(1);
            receiver
        }

        async fn snapshot(
            &self,
            session_id: &leveler_core::SessionId,
        ) -> Result<leveler_client_protocol::UiSessionSnapshot, leveler_client_protocol::ClientError>
        {
            Err(leveler_client_protocol::ClientError::SessionNotFound(
                session_id.clone(),
            ))
        }
    }

    /// D1: `--permission` on an existing session must travel as the
    /// snapshot-versioned setting, carrying the version the CLI observed.
    #[tokio::test]
    async fn an_explicit_permission_change_is_delivered_as_a_versioned_setting() {
        let client = DeliveryOnly {
            delivered: std::sync::Mutex::new(Vec::new()),
        };
        let session_id = leveler_core::SessionId::new("session-a");
        deliver_permission_change(&client, &session_id, PermissionProfile::FullAccess, None, 7)
            .await
            .expect("delivery must use deliver(), not send()");
        let delivered = client.delivered.lock().unwrap();
        let envelope = delivered.first().expect("one envelope");
        assert_eq!(envelope.expected_version, Some(7));
        assert!(
            envelope
                .command
                .requires_snapshot_version()
                .expect("wire type"),
            "set_permission_profile is a snapshot-versioned setting"
        );
        assert!(matches!(
            envelope.command,
            leveler_client_protocol::ClientCommand::SetPermissionProfile { .. }
        ));
    }

    fn layout_with_config(body: &str) -> (Layout, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".leveler")).unwrap();
        std::fs::write(repo.join(".leveler/config.yaml"), body).unwrap();
        let layout = Layout::from_parts(repo, tmp.path().join("configs"), tmp.path().join("state"));
        (layout, tmp)
    }

    /// An existing project that never heard of V2 stays on the legacy
    /// contract. The tier names are identical, so only the contract column
    /// can tell them apart.
    #[test]
    fn a_project_config_without_a_contract_is_v1() {
        let (layout, _tmp) = layout_with_config("mode: assisted\n");
        assert_eq!(
            project_permission_default(&layout),
            (
                PermissionProfile::Assisted,
                leveler_execution::PermissionContract::V1
            )
        );
    }

    #[test]
    fn an_explicit_v2_project_config_is_v2() {
        let (layout, _tmp) = layout_with_config("mode: assisted\npermission_contract: v2\n");
        assert_eq!(
            project_permission_default(&layout),
            (
                PermissionProfile::Assisted,
                leveler_execution::PermissionContract::V2
            )
        );
    }

    /// A legacy mode name is evidence of old data even if the file also says
    /// v2: the name's meaning is fixed by the v1 contract.
    #[test]
    fn a_legacy_mode_name_forces_v1() {
        let (layout, _tmp) = layout_with_config("mode: workspace_write\npermission_contract: v2\n");
        assert_eq!(
            project_permission_default(&layout),
            (
                PermissionProfile::Assisted,
                leveler_execution::PermissionContract::V1
            )
        );
    }

    /// No configured mode means the built-in default for a NEW session, so V2.
    #[test]
    fn a_project_without_a_mode_uses_the_v2_builtin() {
        let (layout, _tmp) = layout_with_config("ignore: []\n");
        assert_eq!(
            project_permission_default(&layout),
            (
                PermissionProfile::Assisted,
                leveler_execution::PermissionContract::V2
            )
        );
    }
}
