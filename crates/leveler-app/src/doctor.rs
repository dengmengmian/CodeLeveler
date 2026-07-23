//! Environment diagnostics for `leveler doctor` (spec §40).
//!
//! Checks tool availability, configuration validity, and per-provider API-key
//! presence. Live provider connectivity is left to `model probe` so `doctor`
//! stays offline and fast.

use std::path::PathBuf;

use leveler_provider::{ProviderConfig, resolve_api_key};

use crate::LoadedConfig;

/// Severity of a diagnostic check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

/// A single diagnostic result.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl CheckResult {
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Ok,
            detail: detail.into(),
        }
    }
    fn warn(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn,
            detail: detail.into(),
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }
}

/// Whether an executable named `name` exists on `PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Run all diagnostics against the loaded config.
///
/// When `memory_dir` is provided (from [`crate::Layout`]), report active and
/// archived memory counts for the project store.
pub fn run(config: &LoadedConfig) -> Vec<CheckResult> {
    run_with_memory(config, None)
}

/// Like [`run`], optionally including a memory-store diagnostic line.
pub fn run_with_memory(
    config: &LoadedConfig,
    memory_dir: Option<&std::path::Path>,
) -> Vec<CheckResult> {
    let mut results = Vec::new();

    // Required tooling.
    for (tool, required) in [("git", true), ("rg", false)] {
        results.push(match find_in_path(tool) {
            Some(p) => CheckResult::ok(tool, p.display().to_string()),
            None if required => CheckResult::fail(tool, "not found on PATH (required)"),
            None => CheckResult::warn(tool, "not found on PATH (optional; falls back to built-in)"),
        });
    }

    // Language toolchains (optional — only needed for that language's projects).
    for tool in ["cargo", "go", "node"] {
        results.push(match find_in_path(tool) {
            Some(p) => CheckResult::ok(tool, p.display().to_string()),
            None => CheckResult::warn(tool, "not found on PATH (optional)"),
        });
    }

    // Config bundle. First install with no models/providers must fail closed —
    // "Environment looks healthy" with zero models is how new users get stuck.
    results.extend(check_model_bundle(config));

    // Surface global-config load failures (incl. MCP plaintext env) even when
    // the rest of doctor runs against a lenient empty LoadedConfig.
    results.push(check_global_config());

    // Per-provider API key.
    for provider in &config.providers {
        results.push(check_api_key(provider));
    }

    // MCP env refs: only report keys / validated source names, never raw values.
    results.extend(check_mcp_env_refs());

    // OS sandbox honesty (WS0): never report sandbox=yes when unsupported.
    let sand = leveler_execution::doctor_sandbox_line();
    let caps = leveler_execution::probe_sandbox_capabilities();
    let status =
        if matches!(caps.write, leveler_execution::FsCapability::Unsupported) && cfg!(windows) {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        };
    results.push(CheckResult {
        name: "sandbox".into(),
        status,
        detail: sand,
    });

    if let Some(dir) = memory_dir {
        results.push(memory_check(dir));
    }

    results
}

fn memory_check(dir: &std::path::Path) -> CheckResult {
    match leveler_memory::MemoryStore::open(dir) {
        Ok(store) => match store.counts() {
            Ok((active, archived)) => CheckResult::ok(
                "memory",
                format!(
                    "memory_dir={} active={} archived={}",
                    dir.display(),
                    active,
                    archived
                ),
            ),
            Err(e) => CheckResult::warn("memory", format!("{} ({e})", dir.display())),
        },
        Err(e) => CheckResult::warn("memory", format!("open {}: {e}", dir.display())),
    }
}

/// Check that at least one provider and model exist, and every model points at
/// a loaded provider. Empty / orphaned configs are Fail so `leveler doctor`
/// never green-lights a machine that cannot start a turn.
fn check_model_bundle(config: &LoadedConfig) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let setup_hint = "add providers/models in ~/.leveler/config.toml (or $LEVELER_HOME/config.toml); see `leveler doctor` / README";

    if config.providers.is_empty() {
        results.push(CheckResult::fail(
            "config: providers",
            format!("none loaded — {setup_hint}"),
        ));
    } else {
        results.push(CheckResult::ok(
            "config: providers",
            format!("{} loaded", config.providers.len()),
        ));
    }

    if config.models.is_empty() {
        results.push(CheckResult::fail(
            "config: models",
            format!("none loaded — {setup_hint}"),
        ));
    } else {
        results.push(CheckResult::ok(
            "config: models",
            format!("{} loaded", config.models.len()),
        ));
        // Orphan models assemble fine but fail at first request with a late
        // "unknown provider" — catch them here for first-install clarity.
        let provider_ids: std::collections::BTreeSet<&str> =
            config.providers.iter().map(|p| p.id.as_str()).collect();
        for model in &config.models {
            if !provider_ids.contains(model.profile.provider.as_str()) {
                results.push(CheckResult::fail(
                    &format!(
                        "config: model {}/{}",
                        model.profile.provider, model.profile.id
                    ),
                    format!(
                        "provider `{}` is not configured — add [providers.{}] in ~/.leveler/config.toml",
                        model.profile.provider, model.profile.provider
                    ),
                ));
            }
        }
    }

    results
}

fn check_api_key(provider: &ProviderConfig) -> CheckResult {
    let name = format!("api key: {}", provider.id);
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
            CheckResult::ok(&name, via)
        }
        Ok(None) => CheckResult::ok(&name, "no key required"),
        Err(_) => CheckResult::fail(
            &name,
            format!("environment variable ${} is not set", provider.api_key_env),
        ),
    }
}

fn check_global_config() -> CheckResult {
    match crate::global_config::GlobalConfig::load() {
        Ok(_) => {
            // Distinguish "file missing → empty defaults" from "file present".
            // First install often has no file; still Ok (models check fails),
            // but the detail must not claim a config exists when it does not.
            let detail = match crate::global_config::GlobalConfig::path() {
                Some(path) if path.is_file() => format!("ok ({})", path.display()),
                Some(path) => format!("not found at {} — using empty defaults", path.display()),
                None => "no path (set HOME, USERPROFILE, or LEVELER_HOME)".into(),
            };
            CheckResult::ok("config: global", detail)
        }
        // Display is key-safe for McpSecretInConfig (names field, not value).
        Err(e) => CheckResult::fail("config: global", e.to_string()),
    }
}

/// MCP servers declare env **name** references only. Warn when a referenced
/// source variable is unset so the server would start without the credential.
/// Never prints raw config values (legacy plaintext must not be echoed).
fn check_mcp_env_refs() -> Vec<CheckResult> {
    let path = match crate::mcp_config::config_path() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    check_mcp_env_refs_at(&path)
}

/// Path-parameterized MCP env checks (unit-testable without process HOME).
fn check_mcp_env_refs_at(path: &std::path::Path) -> Vec<CheckResult> {
    let servers = match crate::mcp_config::list_at(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for s in servers {
        for (key, source) in s.env_keys.iter().zip(s.env_sources.iter()) {
            let name = format!("mcp env: {} / {}", s.name, key);
            // Never interpolate untrusted config values. Only print source when
            // it is a safe UPPER_SNAKE name reference.
            if !crate::mcp_config::is_env_name_ref(source)
                || crate::mcp_config::looks_like_secret_token(source)
            {
                out.push(CheckResult::fail(
                    &name,
                    format!("env `{key}` is not a valid name reference — replace with {key}={key}"),
                ));
                continue;
            }
            match std::env::var(source) {
                Ok(_) => out.push(CheckResult::ok(&name, format!("set via ${source}"))),
                Err(_) => out.push(CheckResult::warn(
                    &name,
                    format!("environment variable ${source} is not set"),
                )),
            }
        }
    }
    out
}

/// Whether any check failed.
pub fn has_failure(results: &[CheckResult]) -> bool {
    results.iter().any(|r| r.status == CheckStatus::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_memory::{MemoryStore, new_entry};

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
                reasoning: Default::default(),
                compatibility: Default::default(),
                instructions: None,
                pricing: None,
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
            retry: Default::default(),
        }
    }

    #[test]
    fn empty_config_fails_closed_for_first_install() {
        let results = run(&LoadedConfig::default());
        assert!(
            has_failure(&results),
            "first install with no models must not look healthy: {results:?}"
        );
        let providers = results
            .iter()
            .find(|r| r.name == "config: providers")
            .expect("providers check");
        let models = results
            .iter()
            .find(|r| r.name == "config: models")
            .expect("models check");
        assert_eq!(providers.status, CheckStatus::Fail, "{}", providers.detail);
        assert_eq!(models.status, CheckStatus::Fail, "{}", models.detail);
        assert!(
            models.detail.contains("config.toml") || models.detail.contains("LEVELER_HOME"),
            "hint must point at global config, not only repo configs/: {}",
            models.detail
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
        let orphan = results
            .iter()
            .find(|r| r.name.contains("deepseek/deepseek-chat"))
            .expect("orphan model check");
        assert_eq!(orphan.status, CheckStatus::Fail, "{}", orphan.detail);
        assert!(orphan.detail.contains("provider"), "{}", orphan.detail);
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
            results.iter().all(|r| r.status == CheckStatus::Ok),
            "{results:?}"
        );
        assert!(!has_failure(&results));
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
            retry: Default::default(),
        };
        let result = check_api_key(&provider);
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn memory_check_reports_counts_and_dir() {
        let root = std::env::temp_dir().join(format!("leveler-doctor-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = MemoryStore::open(&root).unwrap();
        store
            .remember(new_entry("t", "body with unique token xyzzy", vec![]))
            .unwrap();
        let line = memory_check(&root);
        assert_eq!(line.status, CheckStatus::Ok);
        assert!(line.detail.contains("active=1"), "{}", line.detail);
        assert!(line.detail.contains("archived=0"), "{}", line.detail);
        assert!(line.detail.contains("memory_dir="), "{}", line.detail);
        assert!(
            !line.detail.to_lowercase().contains("sandbox=yes"),
            "{}",
            line.detail
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sandbox_doctor_line_never_claims_sandbox_yes() {
        let config = LoadedConfig::default();
        let results = run(&config);
        let sand = results
            .iter()
            .find(|r| r.name == "sandbox")
            .expect("sandbox check");
        assert!(
            !sand.detail.to_lowercase().contains("sandbox=yes"),
            "{}",
            sand.detail
        );
    }

    #[test]
    fn mcp_env_check_reports_keys_not_values() {
        // When no MCP servers are configured (or config path missing), no crash
        // and no secret material appears in diagnostics.
        let results = check_mcp_env_refs();
        for r in &results {
            assert!(
                !r.detail.to_lowercase().contains("sk-"),
                "doctor must not echo secrets: {}",
                r.detail
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
        for r in &results {
            assert!(
                !r.name.contains(secret),
                "check name must not contain secret: {}",
                r.name
            );
            assert!(
                !r.detail.contains(secret),
                "check detail must not contain secret: {}",
                r.detail
            );
            assert!(
                !format!("{r:?}").contains(secret),
                "Debug of CheckResult must not contain secret"
            );
        }
        let legacy = results
            .iter()
            .find(|r| r.name.contains("legacy") && r.name.contains("GITHUB_TOKEN"))
            .expect("legacy mcp env check");
        assert_eq!(legacy.status, CheckStatus::Fail);
        assert!(
            legacy.detail.contains("not a valid name reference"),
            "{}",
            legacy.detail
        );
        assert!(
            legacy.detail.contains("GITHUB_TOKEN=GITHUB_TOKEN"),
            "should suggest the key-only form: {}",
            legacy.detail
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
        let line = results
            .iter()
            .find(|r| r.name.contains("gh") && r.name.contains("GITHUB_TOKEN"))
            .expect("gh env check");
        // Unset in this process → warn naming the source (safe UPPER_SNAKE).
        assert_eq!(line.status, CheckStatus::Warn);
        assert!(line.detail.contains("$GITHUB_TOKEN"), "{}", line.detail);
        std::fs::remove_dir_all(&dir).ok();
    }
}
