//! Environment diagnostics shared by `leveler doctor` and every UI client
//! (spec §40).
//!
//! Every check produces a [`DiagnosticFinding`]: a stable machine id, a typed
//! status, observed evidence, and an optional action key. Wording belongs to
//! the presentation layer, so the CLI and the TUI report the same facts in
//! their own language and never parse each other's text.
//!
//! Checks tool availability, configuration validity, provider key presence,
//! MCP configuration references and OS sandbox capability. Live provider
//! connectivity is left to `model probe` so `doctor` stays offline and fast.
//! Live MCP connection state is not exposed by the runtime yet; that finding
//! says so instead of claiming a connection.

use std::path::PathBuf;

use leveler_client_protocol::{
    DiagnosticCategory, DiagnosticFinding, DiagnosticSeverity, DiagnosticStatus, action_id,
    finding_id,
};
use leveler_provider::{ProviderConfig, resolve_api_key};

use crate::LoadedConfig;

fn checked_at() -> String {
    leveler_core::now().to_rfc3339()
}

fn severity_of(status: DiagnosticStatus) -> DiagnosticSeverity {
    match status {
        DiagnosticStatus::Fail => DiagnosticSeverity::Error,
        DiagnosticStatus::Warn => DiagnosticSeverity::Warning,
        DiagnosticStatus::Pass
        | DiagnosticStatus::Unknown
        | DiagnosticStatus::NotChecked
        | DiagnosticStatus::NotApplicable => DiagnosticSeverity::Info,
    }
}

fn finding(
    id: &str,
    subject: Option<String>,
    category: DiagnosticCategory,
    status: DiagnosticStatus,
    evidence: Vec<String>,
) -> DiagnosticFinding {
    DiagnosticFinding {
        id: id.to_string(),
        subject,
        category,
        severity: severity_of(status),
        status,
        evidence,
        action: None,
        checked_at: checked_at(),
    }
}

fn pass(
    id: &str,
    subject: Option<&str>,
    category: DiagnosticCategory,
    evidence: Vec<String>,
) -> DiagnosticFinding {
    finding(
        id,
        subject.map(str::to_string),
        category,
        DiagnosticStatus::Pass,
        evidence,
    )
}

fn warn(
    id: &str,
    subject: Option<&str>,
    category: DiagnosticCategory,
    evidence: Vec<String>,
) -> DiagnosticFinding {
    finding(
        id,
        subject.map(str::to_string),
        category,
        DiagnosticStatus::Warn,
        evidence,
    )
}

fn fail(
    id: &str,
    subject: Option<&str>,
    category: DiagnosticCategory,
    evidence: Vec<String>,
) -> DiagnosticFinding {
    finding(
        id,
        subject.map(str::to_string),
        category,
        DiagnosticStatus::Fail,
        evidence,
    )
}

fn with_action(mut finding: DiagnosticFinding, action: &str) -> DiagnosticFinding {
    finding.action = Some(action.to_string());
    finding
}

/// Whether an executable named `name` exists on `PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in_path_in(&path, name)
}

/// The injectable half of [`find_in_path`], so lookup rules can be tested
/// without mutating this process's `PATH`.
fn find_in_path_in(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path) {
        for candidate in path_candidates(name) {
            let path = dir.join(&candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// The file names a `PATH` lookup should try.
///
/// Windows executes through `PATHEXT` (`git` resolves to `git.exe`); joining
/// the bare name silently reports every installed Windows tool as missing.
fn path_candidates(name: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let extensions: Vec<&str> = extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .collect();
        let upper = name.to_ascii_uppercase();
        if extensions
            .iter()
            .any(|extension| upper.ends_with(&extension.to_ascii_uppercase()))
        {
            return vec![name.to_string()];
        }
        let mut candidates: Vec<String> = extensions
            .iter()
            .map(|extension| format!("{name}{extension}"))
            .collect();
        // Keep the previous exact-name lookup as a fallback.
        candidates.push(name.to_string());
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![name.to_string()]
    }
}

/// Run diagnostics against the loaded configuration.
pub fn run(config: &LoadedConfig) -> Vec<DiagnosticFinding> {
    run_with_mcp(config, None)
}

/// Run diagnostics with the live MCP connection facts, when the caller has
/// them. `None` means this process is not the runtime that owns the
/// connections (the CLI doctor); the answer then says the check was not run,
/// never that a server is connected.
pub fn run_with_mcp(
    config: &LoadedConfig,
    mcp: Option<&[leveler_client_protocol::mcp::McpServerView]>,
) -> Vec<DiagnosticFinding> {
    run_with_mcp_locale(config, mcp, false)
}

/// The same checks with the runtime's own user-readable evidence rendered for
/// a client that asked for Chinese. Finding ids, statuses, subjects, paths,
/// versions and raw provider errors are unchanged; only the runtime's own
/// sentences are translated (see [`crate::doctor_text`]).
pub fn run_with_mcp_locale(
    config: &LoadedConfig,
    mcp: Option<&[leveler_client_protocol::mcp::McpServerView]>,
    chinese: bool,
) -> Vec<DiagnosticFinding> {
    let mut results = Vec::new();

    // Required tooling.
    for (tool, required) in [("git", true), ("rg", false)] {
        results.push(match find_in_path(tool) {
            Some(p) => pass(
                finding_id::TOOL_AVAILABILITY,
                Some(tool),
                DiagnosticCategory::Configuration,
                vec![p.display().to_string()],
            ),
            None if required => with_action(
                fail(
                    finding_id::TOOL_AVAILABILITY,
                    Some(tool),
                    DiagnosticCategory::Configuration,
                    vec!["not found on PATH (required)".into()],
                ),
                action_id::INSTALL_TOOL,
            ),
            None => with_action(
                warn(
                    finding_id::TOOL_AVAILABILITY,
                    Some(tool),
                    DiagnosticCategory::Configuration,
                    vec!["not found on PATH (optional; falls back to built-in)".into()],
                ),
                action_id::INSTALL_TOOL,
            ),
        });
    }

    // Language toolchains (optional — only needed for that language's projects).
    for tool in ["cargo", "go", "node"] {
        results.push(match find_in_path(tool) {
            Some(p) => pass(
                finding_id::TOOL_AVAILABILITY,
                Some(tool),
                DiagnosticCategory::Configuration,
                vec![p.display().to_string()],
            ),
            None => with_action(
                warn(
                    finding_id::TOOL_AVAILABILITY,
                    Some(tool),
                    DiagnosticCategory::Configuration,
                    vec!["not found on PATH (optional)".into()],
                ),
                action_id::INSTALL_TOOL,
            ),
        });
    }

    // Config bundle. First install with no models/providers must fail closed —
    // "Environment looks healthy" with zero models is how new users get stuck.
    results.extend(check_model_bundle(config));
    results.extend(check_reasoning(config));

    // Surface global-config load failures (incl. MCP plaintext env) even when
    // the rest of doctor runs against a lenient empty LoadedConfig.
    results.push(check_global_config());
    results.push(check_config_reload());
    results.extend(check_retired_model_instructions());

    // Per-provider API key.
    for provider in &config.providers {
        results.push(check_api_key(provider));
    }

    // MCP env refs: only report keys / validated source names, never raw values.
    results.extend(check_mcp_env_refs());
    results.extend(match mcp {
        Some(servers) => check_mcp_connection_states(servers),
        None => vec![check_mcp_connection()],
    });

    // OS sandbox honesty (WS0): never report sandbox=yes when unsupported.
    let sand = leveler_execution::doctor_sandbox_line();
    let caps = leveler_execution::probe_sandbox_capabilities();
    let status =
        if matches!(caps.write, leveler_execution::FsCapability::Unsupported) && cfg!(windows) {
            DiagnosticStatus::Warn
        } else {
            DiagnosticStatus::Pass
        };
    results.push(finding(
        finding_id::SANDBOX_CAPABILITY,
        None,
        DiagnosticCategory::Sandbox,
        status,
        vec![sand],
    ));
    // Capability is not enforcement. No canary ran, so this is never a pass.
    results.push(finding(
        finding_id::SANDBOX_VERIFICATION,
        None,
        DiagnosticCategory::Sandbox,
        DiagnosticStatus::NotChecked,
        vec![
            "no sandbox canary ran in this session; backend capability does not prove enforcement"
                .into(),
        ],
    ));

    if chinese {
        crate::doctor_text::localize_findings(&mut results);
    }
    results
}

/// Agent definitions this project resolves: counts by source, and every
/// invalid definition or unloadable directory by name. Structural only — a
/// model that is not configured is `leveler agents list`'s business.
pub fn agents_check(
    roots: &leveler_agent::agent_registry::AgentRoots,
    trust: &leveler_definition_trust::DefinitionTrustView,
) -> DiagnosticFinding {
    use leveler_agent::agent_registry::{AgentRegistry, AgentSource};
    let registry = AgentRegistry::load(roots, trust);
    let count = |source: AgentSource| {
        registry
            .entries()
            .iter()
            .filter(|e| e.source == source)
            .count()
    };
    let invalid: Vec<&str> = registry
        .entries()
        .iter()
        .filter(|e| e.definition().is_none())
        .map(|e| e.name.as_str())
        .collect();
    let mut evidence = vec![format!(
        "{} project, {} user, {} built-in",
        count(AgentSource::Project),
        count(AgentSource::User),
        count(AgentSource::Builtin)
    )];
    if invalid.is_empty() && registry.problems().is_empty() {
        return pass(
            finding_id::AGENTS_REGISTRY,
            None,
            DiagnosticCategory::Configuration,
            evidence,
        );
    }
    if !invalid.is_empty() {
        evidence.push(format!("{} invalid: {}", invalid.len(), invalid.join(", ")));
    }
    if !registry.problems().is_empty() {
        evidence.push(format!("{} not loaded", registry.problems().len()));
    }
    evidence.push("see `leveler agents list`".into());
    warn(
        finding_id::AGENTS_REGISTRY,
        None,
        DiagnosticCategory::Configuration,
        evidence,
    )
}

/// Every authority-bearing definition with its trust state. A definition that
/// exists but is not approved is a First-Class warning: the user must know why
/// project rules or an agent did not take effect.
pub fn definitions_check(
    roots: &leveler_definition_trust::DiscoveryRoots,
    trust: &leveler_definition_trust::DefinitionTrustView,
) -> DiagnosticFinding {
    use leveler_definition_trust::DefinitionTrust;
    let candidates = leveler_definition_trust::discover(roots);
    if candidates.is_empty() {
        return pass(
            finding_id::DEFINITIONS_TRUST,
            None,
            DiagnosticCategory::Configuration,
            vec!["no authority-bearing definitions on disk".into()],
        );
    }
    let mut pending = Vec::new();
    let mut store_unavailable = false;
    for candidate in &candidates {
        let Some(digest) = &candidate.digest else {
            pending.push(format!(
                "{} [{}] unreadable",
                candidate.identity.display,
                candidate.identity.kind.as_str()
            ));
            continue;
        };
        match trust.check(&candidate.identity, digest) {
            DefinitionTrust::Trusted { .. } | DefinitionTrust::Builtin => {}
            DefinitionTrust::Unapproved => pending.push(format!(
                "{} [{}] new (not trusted)",
                candidate.identity.display,
                candidate.identity.kind.as_str()
            )),
            DefinitionTrust::Changed { .. } => pending.push(format!(
                "{} [{}] changed since approval",
                candidate.identity.display,
                candidate.identity.kind.as_str()
            )),
            DefinitionTrust::StoreUnavailable { status } => {
                store_unavailable = true;
                pending.push(format!(
                    "{} [{}] store unavailable ({})",
                    candidate.identity.display,
                    candidate.identity.kind.as_str(),
                    status.as_str()
                ));
            }
        }
    }
    if pending.is_empty() {
        return pass(
            finding_id::DEFINITIONS_TRUST,
            None,
            DiagnosticCategory::Configuration,
            vec![format!("{} definition(s), all trusted", candidates.len())],
        );
    }
    let mut evidence = vec![format!(
        "{} of {} definition(s) are not admitted",
        pending.len(),
        candidates.len()
    )];
    evidence.extend(pending);
    evidence.push("review with `leveler trust show`, approve with `leveler trust`".into());
    if store_unavailable {
        fail(
            finding_id::DEFINITIONS_TRUST,
            None,
            DiagnosticCategory::Configuration,
            evidence,
        )
    } else {
        warn(
            finding_id::DEFINITIONS_TRUST,
            None,
            DiagnosticCategory::Configuration,
            evidence,
        )
    }
}

/// Check that at least one provider and model exist, and every model points at
/// a loaded provider. Empty / orphaned configs are Fail so `leveler doctor`
/// never green-lights a machine that cannot start a turn.
fn check_model_bundle(config: &LoadedConfig) -> Vec<DiagnosticFinding> {
    let mut results = Vec::new();
    let setup_hint =
        "add providers/models in ~/.leveler/config.toml (or $LEVELER_HOME/config.toml); see README";

    if config.providers.is_empty() {
        results.push(with_action(
            fail(
                finding_id::CONFIG_PROVIDERS,
                None,
                DiagnosticCategory::Configuration,
                vec![format!("none loaded — {setup_hint}")],
            ),
            action_id::CONFIGURE_PROVIDER_MODEL,
        ));
    } else {
        results.push(pass(
            finding_id::CONFIG_PROVIDERS,
            None,
            DiagnosticCategory::Configuration,
            vec![format!("{} loaded", config.providers.len())],
        ));
    }

    if config.models.is_empty() {
        results.push(with_action(
            fail(
                finding_id::CONFIG_MODELS,
                None,
                DiagnosticCategory::Configuration,
                vec![format!("none loaded — {setup_hint}")],
            ),
            action_id::CONFIGURE_PROVIDER_MODEL,
        ));
    } else {
        results.push(pass(
            finding_id::CONFIG_MODELS,
            None,
            DiagnosticCategory::Configuration,
            vec![format!("{} loaded", config.models.len())],
        ));
        // Orphan models assemble fine but fail at first request with a late
        // "unknown provider" — catch them here for first-install clarity.
        let provider_ids: std::collections::BTreeSet<&str> =
            config.providers.iter().map(|p| p.id.as_str()).collect();
        for model in &config.models {
            if !provider_ids.contains(model.profile.provider.as_str()) {
                results.push(with_action(
                    fail(
                        finding_id::CONFIG_MODEL_PROVIDER,
                        Some(&format!(
                            "{}/{}",
                            model.profile.provider, model.profile.id
                        )),
                        DiagnosticCategory::Configuration,
                        vec![format!(
                            "provider `{}` is not configured — add [providers.{}] in ~/.leveler/config.toml",
                            model.profile.provider, model.profile.provider
                        )],
                    ),
                    action_id::CONFIGURE_PROVIDER_MODEL,
                ));
            }
        }
    }

    results
}

fn check_reasoning(config: &LoadedConfig) -> Vec<DiagnosticFinding> {
    use leveler_model::{ReasoningStyle, resolve_reasoning_effort, validate_reasoning_config};

    let mut results = Vec::new();
    for model in &config.models {
        let profile = &model.profile;
        let subject = format!("{}/{}", profile.provider, profile.id);
        if !profile.capabilities.reasoning {
            // A model that does not reason cannot take a Thinking Level. The
            // global default reaches every model, so this is where a config that
            // asks for one here is reported instead of being silently dropped.
            if let Some(level) = profile
                .thinking
                .filter(|level| *level != leveler_model::ThinkingLevel::Auto)
            {
                results.push(with_action(
                    warn(
                        finding_id::MODEL_THINKING,
                        Some(&subject),
                        DiagnosticCategory::Model,
                        vec![format!(
                            "thinking = \"{level}\" has no effect: this model does not reason; set this model's thinking to \"auto\" to use its native behavior"
                        )],
                    ),
                    action_id::FIX_CONFIG,
                ));
            }
            continue;
        }
        if let Err(reason) = validate_reasoning_config(true, &profile.reasoning) {
            results.push(with_action(
                fail(
                    finding_id::MODEL_REASONING,
                    Some(&subject),
                    DiagnosticCategory::Model,
                    vec![reason],
                ),
                action_id::FIX_CONFIG,
            ));
            continue;
        }
        // The user's configured level against what this model can actually
        // distinguish. A level the model cannot express is never rounded into a
        // neighbouring one, so the honest report is the real option list.
        if let Some(level) = profile.thinking {
            let caps = leveler_model::ThinkingCapabilities::of(
                profile.capabilities.reasoning,
                &profile.reasoning,
            );
            if level != leveler_model::ThinkingLevel::Auto && !caps.accepts(level) {
                let available = if caps.levels().is_empty() {
                    "this model's thinking cannot be adjusted".to_string()
                } else {
                    format!(
                        "available: {}",
                        caps.levels()
                            .iter()
                            .map(|level| level.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                results.push(with_action(
                    warn(
                        finding_id::MODEL_THINKING,
                        Some(&subject),
                        DiagnosticCategory::Model,
                        vec![format!(
                            "thinking = \"{level}\" is not available on this model ({available}); it will use the provider's own default instead"
                        )],
                    ),
                    action_id::FIX_CONFIG,
                ));
            }
        }
        let resolved = resolve_reasoning_effort(None, &profile.reasoning);
        let evidence = match profile.reasoning.style {
            ReasoningStyle::None => {
                vec!["always-on (no effort knob); CodeLeveler default n/a".to_string()]
            }
            ReasoningStyle::BudgetedThinking { budget_tokens } => {
                vec![format!("explicit thinking budget={budget_tokens}")]
            }
            ReasoningStyle::OpenAiEffort
            | ReasoningStyle::ThinkingFlag
            | ReasoningStyle::AdaptiveThinking => {
                let supported = profile
                    .reasoning
                    .supported_efforts
                    .iter()
                    .map(|e| e.as_wire())
                    .collect::<Vec<_>>()
                    .join(", ");
                let default = profile
                    .reasoning
                    .default_effort
                    .map(|e| e.as_wire())
                    .unwrap_or("missing");
                match resolved.effective {
                    Some(effective) => vec![format!(
                        "supported=[{supported}] default={default} effective={}",
                        effective.as_wire()
                    )],
                    None => {
                        results.push(with_action(
                            fail(
                                finding_id::MODEL_REASONING,
                                Some(&subject),
                                DiagnosticCategory::Model,
                                vec![format!(
                                    "reasoning=true but effective effort is unknown (supported=[{supported}])"
                                )],
                            ),
                            action_id::FIX_CONFIG,
                        ));
                        continue;
                    }
                }
            }
        };
        results.push(pass(
            finding_id::MODEL_REASONING,
            Some(&subject),
            DiagnosticCategory::Model,
            evidence,
        ));
    }
    results
}

fn check_api_key(provider: &ProviderConfig) -> DiagnosticFinding {
    match resolve_api_key(provider) {
        Ok(Some(_)) => {
            let via = if provider
                .api_key
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                "set via config (api_key)".to_string()
            } else {
                format!("set via ${}", provider.api_key_env)
            };
            pass(
                finding_id::PROVIDER_API_KEY,
                Some(&provider.id),
                DiagnosticCategory::Model,
                vec![via],
            )
        }
        Ok(None) => pass(
            finding_id::PROVIDER_API_KEY,
            Some(&provider.id),
            DiagnosticCategory::Model,
            vec!["no key required".into()],
        ),
        Err(_) => with_action(
            fail(
                finding_id::PROVIDER_API_KEY,
                Some(&provider.id),
                DiagnosticCategory::Model,
                vec![format!(
                    "environment variable ${} is not set",
                    provider.api_key_env
                )],
            ),
            action_id::SET_PROVIDER_KEY,
        ),
    }
}

fn check_global_config() -> DiagnosticFinding {
    match crate::global_config::GlobalConfig::load() {
        Ok(_) => {
            // Distinguish "file missing → empty defaults" from "file present".
            // First install often has no file; still a pass here (the models
            // check fails), but the evidence must not claim a config exists
            // when it does not.
            let evidence = match crate::global_config::GlobalConfig::path() {
                Some(path) if path.is_file() => vec![format!("ok ({})", path.display())],
                Some(path) => vec![format!(
                    "not found at {} — using empty defaults",
                    path.display()
                )],
                None => vec!["no path (set HOME, USERPROFILE, or LEVELER_HOME)".into()],
            };
            pass(
                finding_id::CONFIG_GLOBAL,
                None,
                DiagnosticCategory::Configuration,
                evidence,
            )
        }
        // Display is key-safe for McpSecretInConfig (names field, not value).
        Err(e) => with_action(
            fail(
                finding_id::CONFIG_GLOBAL,
                None,
                DiagnosticCategory::Configuration,
                vec![e.to_string()],
            ),
            action_id::FIX_CONFIG,
        ),
    }
}

/// Reload generation is not part of the client protocol. The active source
/// and its parse result are checked above; saying what is not observable is
/// the honest answer, not a fabricated "reload ok".
fn check_config_reload() -> DiagnosticFinding {
    finding(
        finding_id::CONFIG_RELOAD,
        None,
        DiagnosticCategory::Configuration,
        DiagnosticStatus::NotChecked,
        vec![
            "the runtime does not expose a configuration reload generation to clients; the active source and parse result are checked above"
                .into(),
        ],
    )
}

/// A retired key must not be ignored in silence: an old config that still
/// carries `instructions` would look like it was applying a per-model prompt
/// when every model now shares the base prompt.
fn check_retired_model_instructions() -> Vec<DiagnosticFinding> {
    let Ok(config) = crate::global_config::GlobalConfig::load() else {
        // `check_global_config` already reports the load failure.
        return Vec::new();
    };
    retired_model_instruction_warnings(&config)
}

/// The warning rows for a loaded config, split out so the message is testable
/// without process-global `LEVELER_HOME`.
fn retired_model_instruction_warnings(
    config: &crate::global_config::GlobalConfig,
) -> Vec<DiagnosticFinding> {
    config
        .retired_model_instructions()
        .into_iter()
        .map(|id| {
            with_action(
                warn(
                    finding_id::CONFIG_RETIRED_INSTRUCTIONS,
                    Some(&id),
                    DiagnosticCategory::Configuration,
                    vec![
                        "model.instructions is retired and ignored; all models now share the \
                         CodeLeveler base prompt — remove the key"
                            .into(),
                    ],
                ),
                action_id::FIX_CONFIG,
            )
        })
        .collect()
}

/// MCP servers declare env **name** references only. Warn when a referenced
/// source variable is unset so the server would start without the credential.
/// Never prints raw config values (legacy plaintext must not be echoed).
fn check_mcp_env_refs() -> Vec<DiagnosticFinding> {
    let path = match crate::mcp_config::config_path() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    check_mcp_env_refs_at(&path)
}

/// Path-parameterized MCP env checks (unit-testable without process HOME).
fn check_mcp_env_refs_at(path: &std::path::Path) -> Vec<DiagnosticFinding> {
    let servers = match crate::mcp_config::list_at(path) {
        Ok(s) => s,
        Err(_) => {
            // The parse error may quote a legacy plaintext value, so the
            // evidence states the failure without reproducing it.
            return vec![with_action(
                fail(
                    finding_id::MCP_CONFIG,
                    None,
                    DiagnosticCategory::Mcp,
                    vec!["MCP server configuration could not be read (I/O or parse error)".into()],
                ),
                action_id::REVIEW_MCP_CONFIG,
            )];
        }
    };
    let mut out = Vec::new();
    for s in servers {
        for (key, source) in s.env_keys.iter().zip(s.env_sources.iter()) {
            push_mcp_ref(&mut out, &s.name, "env", key, source);
        }
        for (key, source) in s.header_names.iter().zip(s.header_sources.iter()) {
            push_mcp_ref(&mut out, &s.name, "header", key, source);
        }
        if let Some(source) = &s.bearer_token_env {
            push_mcp_ref(&mut out, &s.name, "bearer", "bearer_token_env", source);
        }
    }
    out
}

fn push_mcp_ref(
    out: &mut Vec<DiagnosticFinding>,
    server: &str,
    kind: &str,
    key: &str,
    source: &str,
) {
    let subject = format!("{server}/{key}");
    // Never interpolate untrusted config values. Only print source when
    // it is a safe UPPER_SNAKE name reference.
    if !crate::mcp_config::is_env_name_ref(source)
        || crate::mcp_config::looks_like_secret_token(source)
    {
        let detail = if kind == "env" {
            format!("env `{key}` is not a valid name reference — replace with {key}={key}")
        } else {
            format!(
                "`{key}` is not a valid name reference — replace the value with an UPPER_SNAKE env name"
            )
        };
        out.push(with_action(
            fail(
                finding_id::MCP_CONFIG,
                Some(&subject),
                DiagnosticCategory::Mcp,
                vec![detail],
            ),
            action_id::REVIEW_MCP_CONFIG,
        ));
        return;
    }
    match std::env::var(source) {
        Ok(_) => out.push(pass(
            finding_id::MCP_CONFIG,
            Some(&subject),
            DiagnosticCategory::Mcp,
            vec![format!("set via ${source}")],
        )),
        Err(_) => out.push(with_action(
            warn(
                finding_id::MCP_CONFIG,
                Some(&subject),
                DiagnosticCategory::Mcp,
                vec![format!("environment variable ${source} is not set")],
            ),
            action_id::REVIEW_MCP_CONFIG,
        )),
    }
}

/// Live MCP connection state when the caller is not the owning runtime.
///
/// The CLI doctor runs in its own process and cannot see the daemon's
/// connections. Saying the check was not run is the honest answer; it never
/// becomes a pass and never claims a server is connected.
fn check_mcp_connection() -> DiagnosticFinding {
    let count = crate::mcp_config::config_path()
        .ok()
        .and_then(|path| crate::mcp_config::list_at(&path).ok())
        .map(|servers| servers.len())
        .unwrap_or(0);
    if count == 0 {
        return finding(
            finding_id::MCP_CONNECTION,
            None,
            DiagnosticCategory::Mcp,
            DiagnosticStatus::NotApplicable,
            vec!["no MCP servers configured".into()],
        );
    }
    finding(
        finding_id::MCP_CONNECTION,
        None,
        DiagnosticCategory::Mcp,
        DiagnosticStatus::NotChecked,
        vec![format!(
            "{count} server(s) configured; this process does not own their connections"
        )],
    )
}

/// One finding per configured server, from the same runtime authority `/mcp`
/// reads. A server that only exists in configuration is never reported as
/// connected.
fn check_mcp_connection_states(
    servers: &[leveler_client_protocol::mcp::McpServerView],
) -> Vec<DiagnosticFinding> {
    use leveler_client_protocol::mcp::McpConnectionState as State;
    if servers.is_empty() {
        return vec![finding(
            finding_id::MCP_CONNECTION,
            None,
            DiagnosticCategory::Mcp,
            DiagnosticStatus::NotApplicable,
            vec!["no MCP servers configured".into()],
        )];
    }
    servers
        .iter()
        .map(|server| {
            let (status, evidence) = match server.state {
                State::Disabled => (
                    DiagnosticStatus::NotApplicable,
                    "disabled by configuration".to_string(),
                ),
                State::NotConnected => (
                    DiagnosticStatus::Warn,
                    format!(
                        "configured but not connected ({}); open /mcp to connect",
                        server.endpoint
                    ),
                ),
                State::Connecting => (
                    DiagnosticStatus::Unknown,
                    "a connection attempt is in progress".to_string(),
                ),
                State::Connected => {
                    let mut detail = format!(
                        "connected · {} tool(s)",
                        server.tool_count.unwrap_or(0)
                    );
                    if let Some(protocol) = &server.protocol {
                        detail.push_str(&format!(" · protocol {protocol}"));
                    }
                    (DiagnosticStatus::Pass, detail)
                }
                State::Degraded => (
                    DiagnosticStatus::Warn,
                    "the previous connection is no longer serving its catalog; open /mcp to reconnect"
                        .to_string(),
                ),
                State::Failed => (
                    DiagnosticStatus::Fail,
                    match &server.last_error {
                        Some(reason) => format!("last attempt failed: {reason}"),
                        None => "last attempt failed".to_string(),
                    },
                ),
                State::Unknown => (
                    DiagnosticStatus::Unknown,
                    "the runtime cannot observe this server".to_string(),
                ),
            };
            let needs_action = matches!(
                server.state,
                State::NotConnected | State::Degraded | State::Failed
            );
            let found = finding(
                finding_id::MCP_CONNECTION,
                Some(server.name.clone()),
                DiagnosticCategory::Mcp,
                status,
                vec![evidence],
            );
            if needs_action {
                with_action(found, action_id::OPEN_MCP)
            } else {
                found
            }
        })
        .collect()
}

/// Whether any check failed.
pub fn has_failure(results: &[DiagnosticFinding]) -> bool {
    results
        .iter()
        .any(|finding| finding.status == DiagnosticStatus::Fail)
}

/// English title for one finding id, used by the CLI's text output.
///
/// UI clients carry their own localized table; this function exists only so
/// the CLI does not print bare machine keys.
pub fn label(id: &str) -> &'static str {
    match id {
        finding_id::RUNTIME_CONNECTION => "runtime connection",
        finding_id::RUNTIME_BUILD => "runtime build",
        finding_id::RUNTIME_SESSION => "runtime session",
        finding_id::RUNTIME_RECOVERY => "runtime recovery",
        finding_id::TOOL_AVAILABILITY => "tool",
        finding_id::CONFIG_PROVIDERS => "config: providers",
        finding_id::CONFIG_MODELS => "config: models",
        finding_id::CONFIG_MODEL_PROVIDER => "config: model provider",
        finding_id::CONFIG_GLOBAL => "config: global",
        finding_id::CONFIG_RELOAD => "config: reload",
        finding_id::CONFIG_RETIRED_INSTRUCTIONS => "config: retired instructions",
        finding_id::MODEL_REASONING => "reasoning",
        finding_id::MODEL_THINKING => "thinking",
        finding_id::PROVIDER_API_KEY => "api key",
        finding_id::AGENTS_REGISTRY => "agents",
        finding_id::MCP_CONFIG => "mcp config",
        finding_id::MCP_CONNECTION => "mcp connection",
        finding_id::PERMISSION_PROFILE => "permission",
        finding_id::SANDBOX_CAPABILITY => "sandbox capability",
        finding_id::SANDBOX_VERIFICATION => "sandbox verification",
        finding_id::TERMINAL_CAPABILITY => "terminal",
        _ => "diagnostic",
    }
}

/// English sentence for an action key, used by the CLI's text output.
pub fn action_label(action: &str) -> &'static str {
    match action {
        action_id::CONFIGURE_PROVIDER_MODEL => "add a provider and a model in the global config",
        action_id::SET_PROVIDER_KEY => "set the provider's API key environment variable",
        action_id::FIX_CONFIG => "review the referenced configuration entry",
        action_id::INSTALL_TOOL => "install the tool or add it to PATH",
        action_id::REVIEW_MCP_CONFIG => "review the MCP server configuration",
        action_id::REVIEW_PERMISSION => "review the session's permission profile",
        action_id::ACKNOWLEDGE_RECOVERY => "review and acknowledge the crash window",
        action_id::RESTART_TO_MATCH => "restart the app to match the runtime build",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::DiagnosticCategory;

    /// Windows resolves `git` through `PATHEXT`; a bare `PATH` join reports
    /// every installed tool as missing.
    #[cfg(windows)]
    #[test]
    fn a_windows_path_lookup_applies_pathext() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("leveler-doctor-probe.exe");
        std::fs::write(&tool, b"stub").unwrap();
        let path = dir.path().as_os_str();
        let found = find_in_path_in(path, "leveler-doctor-probe").expect("found via PATHEXT");
        assert!(found.is_file());
        assert_eq!(
            found
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_ascii_lowercase(),
            "leveler-doctor-probe.exe"
        );
        assert_eq!(
            find_in_path_in(path, "leveler-doctor-probe.exe"),
            Some(tool)
        );
        assert_eq!(find_in_path_in(path, "leveler-doctor-missing"), None);
    }

    fn find<'a>(
        results: &'a [DiagnosticFinding],
        id: &str,
        subject: &str,
    ) -> &'a DiagnosticFinding {
        results
            .iter()
            .find(|finding| finding.id == id && finding.subject.as_deref() == Some(subject))
            .unwrap_or_else(|| panic!("missing {id}:{subject}: {results:?}"))
    }

    fn find_any<'a>(results: &'a [DiagnosticFinding], id: &str) -> &'a DiagnosticFinding {
        results
            .iter()
            .find(|finding| finding.id == id)
            .unwrap_or_else(|| panic!("missing {id}: {results:?}"))
    }

    /// A retired per-model `instructions` key must produce a visible doctor
    /// warning instead of being silently ignored.
    #[test]
    fn the_retired_instructions_key_produces_a_doctor_warning() {
        let config: crate::global_config::GlobalConfig =
            toml::from_str("[models.m]\nprovider = \"p\"\ninstructions = \"be special\"\n")
                .unwrap();
        let warnings = retired_model_instruction_warnings(&config);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].id, finding_id::CONFIG_RETIRED_INSTRUCTIONS);
        assert_eq!(warnings[0].status, DiagnosticStatus::Warn);
        assert!(
            warnings[0]
                .evidence
                .join(" ")
                .contains("retired and ignored")
        );

        let clean: crate::global_config::GlobalConfig =
            toml::from_str("[models.m]\nprovider = \"p\"\n").unwrap();
        assert!(retired_model_instruction_warnings(&clean).is_empty());
    }

    fn sample_model(provider: &str, id: &str) -> leveler_provider::ModelConfigFile {
        use leveler_model::{ModelCapabilities, ModelLimits, ModelProfile, ProtocolKind};
        leveler_provider::ModelConfigFile {
            profile: ModelProfile {
                id: id.into(),
                provider: provider.into(),
                model_id: id.into(),
                protocol: ProtocolKind::OpenAiChat,
                capabilities: ModelCapabilities {
                    streaming: true,
                    tool_calling: true,
                    parallel_tool_calls: true,
                    structured_output: true,
                    reasoning: false,
                    vision: false,
                },
                limits: ModelLimits {
                    context_window: 8_192,
                    reliable_context: 4_096,
                    max_output_tokens: 1_024,
                    max_tool_schema_bytes: 8_192,
                    max_parallel_tool_calls: 1,
                    max_tool_output_bytes: None,
                },
                context_quality: None,
                reasoning: Default::default(),
                compatibility: Default::default(),
                pricing: None,
                thinking: None,
            },
            policy: None,
        }
    }

    fn sample_provider(id: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            protocol: leveler_model::ProtocolKind::OpenAiChat,
            base_url: "https://provider.invalid".into(),
            api_key_env: "DEFINITELY_UNSET_LEVELER_TEST_KEY".into(),
            api_key: Some("sk-test".into()),
            headers: Default::default(),
            timeouts: Default::default(),
        }
    }

    #[test]
    fn default_high_is_diagnosed_for_unsupported_fixed_and_mismatched_models() {
        let config: crate::global_config::GlobalConfig = toml::from_str(
            r#"
            [models.unsupported]
            provider = "p"
            [models.fixed]
            provider = "p"
            reasoning = true
            [models.mismatch]
            provider = "p"
            reasoning = true
            reasoning_style = "open_ai_effort"
            supported_efforts = ["low", "medium"]
            reasoning_effort = "low"
            "#,
        )
        .unwrap();
        let bundle = config.into_bundle();
        let loaded = LoadedConfig {
            models: bundle.models,
            ..Default::default()
        };
        let results = check_reasoning(&loaded);
        for id in ["unsupported", "fixed", "mismatch"] {
            let warning = find(&results, finding_id::MODEL_THINKING, &format!("p/{id}"));
            assert_eq!(warning.status, DiagnosticStatus::Warn);
            assert!(
                warning.evidence.join(" ").contains("thinking = \"high\""),
                "{warning:?}"
            );
        }
    }

    #[test]
    fn chinese_evidence_is_localized_without_changing_findings_or_facts() {
        let config = LoadedConfig::default();
        let english = run_with_mcp_locale(&config, None, false);
        let chinese = run_with_mcp_locale(&config, None, true);
        let shape = |findings: &[DiagnosticFinding]| {
            findings
                .iter()
                .map(|finding| (finding.id.clone(), finding.subject.clone(), finding.status))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shape(&english),
            shape(&chinese),
            "the report structure and statuses do not depend on the display language"
        );
        assert!(
            english
                .iter()
                .flat_map(|finding| &finding.evidence)
                .any(|line| line.starts_with("none loaded")),
            "{english:?}"
        );
        assert!(
            chinese
                .iter()
                .flat_map(|finding| &finding.evidence)
                .any(|line| line.starts_with("未加载")),
            "{chinese:?}"
        );
        // Machine facts stay in their original form.
        assert!(
            chinese
                .iter()
                .flat_map(|finding| &finding.evidence)
                .any(|line| line.contains("write=")),
            "the sandbox capability line is a technical fact: {chinese:?}"
        );
    }

    #[test]
    fn explicit_auto_does_not_warn_on_unsupported_or_fixed_models() {
        for reasoning in [false, true] {
            let mut model = sample_model("p", "m");
            model.profile.capabilities.reasoning = reasoning;
            model.profile.thinking = Some(leveler_model::ThinkingLevel::Auto);
            let config = LoadedConfig {
                models: vec![model],
                ..Default::default()
            };
            assert!(
                !check_reasoning(&config)
                    .iter()
                    .any(|finding| finding.id == finding_id::MODEL_THINKING),
                "Auto asks for no adjustment"
            );
        }
    }

    #[test]
    fn empty_config_fails_closed_for_first_install() {
        let results = run(&LoadedConfig::default());
        assert!(
            has_failure(&results),
            "first install with no models must not look healthy: {results:?}"
        );
        let providers = find_any(&results, finding_id::CONFIG_PROVIDERS);
        let models = find_any(&results, finding_id::CONFIG_MODELS);
        assert_eq!(providers.status, DiagnosticStatus::Fail, "{providers:?}");
        assert_eq!(models.status, DiagnosticStatus::Fail, "{models:?}");
        assert!(
            models.evidence.join(" ").contains("config.toml")
                || models.evidence.join(" ").contains("LEVELER_HOME"),
            "hint must point at global config, not only repo configs/: {models:?}"
        );
    }

    #[test]
    fn model_without_matching_provider_is_a_failure() {
        let config = LoadedConfig {
            providers: vec![],
            models: vec![sample_model("deepseek", "deepseek-chat")],
            ..Default::default()
        };
        let results = run(&config);
        assert!(has_failure(&results));
        let orphan = find(
            &results,
            finding_id::CONFIG_MODEL_PROVIDER,
            "deepseek/deepseek-chat",
        );
        assert_eq!(orphan.status, DiagnosticStatus::Fail, "{orphan:?}");
        assert!(orphan.evidence.join(" ").contains("provider"), "{orphan:?}");
    }

    #[test]
    fn complete_bundle_passes_provider_and_model_checks() {
        let config = LoadedConfig {
            providers: vec![sample_provider("deepseek")],
            models: vec![sample_model("deepseek", "deepseek-chat")],
            ..Default::default()
        };
        let results = check_model_bundle(&config);
        assert!(
            results.iter().all(|r| r.status == DiagnosticStatus::Pass),
            "{results:?}"
        );
        assert!(!has_failure(&results));
    }

    #[test]
    fn reasoning_knob_model_is_reported() {
        let mut model = sample_model("deepseek", "deepseek-v4-pro");
        model.profile.capabilities.reasoning = true;
        model.profile.reasoning = leveler_model::ReasoningConfig {
            style: leveler_model::ReasoningStyle::ThinkingFlag,
            supported_efforts: vec![
                leveler_model::ReasoningEffort::High,
                leveler_model::ReasoningEffort::Max,
            ],
            default_effort: Some(leveler_model::ReasoningEffort::Max),
            preserved_thinking: None,
            forced: false,
        };
        let config = LoadedConfig {
            providers: vec![sample_provider("deepseek")],
            models: vec![model],
            ..Default::default()
        };
        let results = check_reasoning(&config);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, DiagnosticStatus::Pass);
        let evidence = results[0].evidence.join(" ");
        assert!(evidence.contains("default=max"), "{evidence}");
        assert!(evidence.contains("effective=max"), "{evidence}");
    }

    /// A Thinking Level the model cannot express is reported with the levels it
    /// can, because the harness will not round the user's choice for them.
    #[test]
    fn an_unexpressible_thinking_level_is_reported_with_the_real_options() {
        let mut model = sample_model("deepseek", "deepseek-v4-pro");
        model.profile.capabilities.reasoning = true;
        model.profile.reasoning = leveler_model::ReasoningConfig {
            style: leveler_model::ReasoningStyle::OpenAiEffort,
            supported_efforts: vec![
                leveler_model::ReasoningEffort::Low,
                leveler_model::ReasoningEffort::High,
            ],
            default_effort: Some(leveler_model::ReasoningEffort::High),
            preserved_thinking: None,
            forced: false,
        };
        // A global default that this model cannot distinguish.
        model.profile.thinking = Some(leveler_model::ThinkingLevel::Medium);
        let config = LoadedConfig {
            providers: vec![sample_provider("deepseek")],
            models: vec![model],
            ..Default::default()
        };
        let results = check_reasoning(&config);
        let warning = find(
            &results,
            finding_id::MODEL_THINKING,
            "deepseek/deepseek-v4-pro",
        );
        assert_eq!(warning.status, DiagnosticStatus::Warn, "{warning:?}");
        let evidence = warning.evidence.join(" ");
        assert!(evidence.contains("thinking = \"medium\""), "{evidence}");
        assert!(
            evidence.contains("auto, low, max"),
            "the real options, deduplicated exactly as the selector shows them: {evidence}"
        );
        assert!(
            evidence.contains("provider's own default"),
            "and what happens instead: {evidence}"
        );
    }

    /// A level set against a model that does not reason at all is reported too,
    /// rather than being dropped without a word.
    #[test]
    fn a_thinking_level_on_a_non_reasoning_model_is_reported() {
        let mut model = sample_model("deepseek", "deepseek-chat");
        model.profile.capabilities.reasoning = false;
        model.profile.thinking = Some(leveler_model::ThinkingLevel::Max);
        let config = LoadedConfig {
            providers: vec![sample_provider("deepseek")],
            models: vec![model],
            ..Default::default()
        };
        let results = check_reasoning(&config);
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].status, DiagnosticStatus::Warn);
        assert!(
            results[0].evidence.join(" ").contains("does not reason"),
            "{results:?}"
        );
    }

    #[test]
    fn reports_missing_provider_api_key_as_failure() {
        let provider = ProviderConfig {
            id: "deepseek".into(),
            protocol: leveler_model::ProtocolKind::OpenAiChat,
            // Dummy fixture: this test never makes a network call — it only
            // checks that an unset key is reported as a failure.
            base_url: "https://provider.invalid".into(),
            api_key_env: "DEFINITELY_UNSET_LEVELER_TEST_KEY".into(),
            api_key: None,
            headers: Default::default(),
            timeouts: Default::default(),
        };
        let result = check_api_key(&provider);
        assert_eq!(result.status, DiagnosticStatus::Fail);
        assert_eq!(result.category, DiagnosticCategory::Model);
    }

    #[test]
    fn sandbox_doctor_line_never_claims_sandbox_yes() {
        let config = LoadedConfig::default();
        let results = run(&config);
        let sand = find_any(&results, finding_id::SANDBOX_CAPABILITY);
        assert!(
            !sand
                .evidence
                .join(" ")
                .to_lowercase()
                .contains("sandbox=yes"),
            "{sand:?}"
        );
    }

    #[test]
    fn mcp_env_check_reports_keys_not_values() {
        // When no MCP servers are configured (or config path missing), no crash
        // and no secret material appears in diagnostics.
        let results = check_mcp_env_refs();
        for finding in &results {
            assert!(
                !finding.evidence.join(" ").to_lowercase().contains("sk-"),
                "doctor must not echo secrets: {finding:?}"
            );
        }
    }

    #[test]
    fn mcp_env_check_never_echoes_legacy_plaintext_source() {
        let dir = std::env::temp_dir().join(format!("leveler-doctor-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        let secret = "ghp_legacyDoctorMustNotEchoThisSecret99";
        std::fs::write(
            &cfg,
            format!(
                r#"
[[mcp_servers]]
name = "legacy"
command = "echo"
env = {{ GITHUB_TOKEN = "{secret}" }}
"#
            ),
        )
        .unwrap();

        let results = check_mcp_env_refs_at(&cfg);
        assert!(
            !results.is_empty(),
            "expected a failure for the legacy secret source"
        );
        for finding in &results {
            assert!(
                !finding.evidence.join(" ").contains(secret),
                "evidence must not contain secret: {finding:?}"
            );
            assert!(
                !format!("{finding:?}").contains(secret),
                "Debug of DiagnosticFinding must not contain secret"
            );
        }
        let legacy = find(&results, finding_id::MCP_CONFIG, "legacy/GITHUB_TOKEN");
        assert_eq!(legacy.status, DiagnosticStatus::Fail);
        let evidence = legacy.evidence.join(" ");
        assert!(
            evidence.contains("not a valid name reference"),
            "{evidence}"
        );
        assert!(
            evidence.contains("GITHUB_TOKEN=GITHUB_TOKEN"),
            "should suggest the key-only form: {evidence}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mcp_env_check_accepts_upper_snake_source_without_echoing_unset_value() {
        let dir =
            std::env::temp_dir().join(format!("leveler-doctor-mcp-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(
            &cfg,
            r#"
[[mcp_servers]]
name = "gh"
command = "echo"
env = { GITHUB_TOKEN = "GITHUB_TOKEN" }
"#,
        )
        .unwrap();
        let results = check_mcp_env_refs_at(&cfg);
        let line = find(&results, finding_id::MCP_CONFIG, "gh/GITHUB_TOKEN");
        // Unset in this process → warn naming the source (safe UPPER_SNAKE).
        assert_eq!(line.status, DiagnosticStatus::Warn);
        assert!(
            line.evidence.join(" ").contains("$GITHUB_TOKEN"),
            "{line:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandbox_capability_never_stands_in_for_enforcement() {
        let results = run(&LoadedConfig::default());
        let status = find_any(&results, finding_id::SANDBOX_CAPABILITY).status;
        assert!(
            matches!(status, DiagnosticStatus::Pass | DiagnosticStatus::Warn),
            "{status:?}"
        );
        let verification = find_any(&results, finding_id::SANDBOX_VERIFICATION);
        assert_eq!(verification.status, DiagnosticStatus::NotChecked);
        assert!(
            verification
                .evidence
                .join(" ")
                .contains("does not prove enforcement"),
            "{verification:?}"
        );
    }

    #[test]
    fn every_finding_uses_a_stable_id_and_a_typed_status() {
        let config = LoadedConfig {
            providers: vec![sample_provider("deepseek")],
            models: vec![sample_model("deepseek", "deepseek-chat")],
            ..Default::default()
        };
        let results = run(&config);
        assert!(!results.is_empty());
        for finding in &results {
            assert!(!finding.id.is_empty());
            assert!(
                !finding.id.contains(' '),
                "ids are machine keys, not sentences: {}",
                finding.id
            );
            assert!(!finding.checked_at.is_empty());
            assert_eq!(
                finding.severity,
                severity_of(finding.status),
                "severity is derived from status, not guessed: {finding:?}"
            );
        }
    }
}
