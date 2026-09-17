//! Manage MCP servers in the resident global config (`~/.leveler/config.toml`)
//! without hand-editing TOML — the `mcp add/list/remove` flow.
//! Edits are format-preserving (comments and unrelated tables survive).
//!
//! MCP `env` entries store **environment variable name references only**
//! (never cleartext secret values). At runtime the app resolves each reference
//! from the process environment.

use std::path::PathBuf;

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use crate::global_config::GlobalConfig;

/// One configured MCP server, as shown by `list`.
#[derive(Debug, Clone)]
pub struct McpEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Destination env keys forwarded into the MCP process (display only).
    pub env_keys: Vec<String>,
    /// Source env var names corresponding to [`Self::env_keys`] (same order).
    /// Doctor checks these; values are never displayed.
    pub env_sources: Vec<String>,
    /// Whether `command` is found on `PATH`.
    pub available: bool,
}

/// Errors from editing the MCP config.
#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    #[error("cannot locate the config path (set HOME or LEVELER_HOME)")]
    NoPath,
    #[error("config is not valid TOML: {0}")]
    Parse(String),
    #[error("an MCP server named `{0}` already exists")]
    Exists(String),
    #[error("no MCP server named `{0}`")]
    NotFound(String),
    #[error(
        "MCP env `{key}` looks like a plaintext secret; store an environment variable name reference instead (e.g. `{key}={key}` or `{key}=`)"
    )]
    SecretValue { key: String },
    #[error(
        "MCP env `{key}` source is not a valid UPPER_SNAKE name reference (use `{key}=`, `{key}={key}`, or `{key}=$OTHER_ENV`)"
    )]
    InvalidSource { key: String },
    #[error("MCP env key `{0}` is not a valid UPPER_SNAKE environment variable name")]
    InvalidKey(String),
    #[error("io error: {0}")]
    Io(String),
}

/// The resident config path (`$LEVELER_HOME/config.toml` or `~/.leveler/config.toml`).
pub fn config_path() -> Result<PathBuf, McpConfigError> {
    GlobalConfig::path().ok_or(McpConfigError::NoPath)
}

fn document_at(path: &std::path::Path) -> Result<DocumentMut, McpConfigError> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.parse::<DocumentMut>()
        .map_err(|e| McpConfigError::Parse(e.to_string()))
}

/// List configured MCP servers.
pub fn list() -> Result<Vec<McpEntry>, McpConfigError> {
    list_at(&config_path()?)
}

/// List servers from a specific config file.
pub fn list_at(path: &std::path::Path) -> Result<Vec<McpEntry>, McpConfigError> {
    let doc = document_at(path)?;
    let mut out = Vec::new();
    if let Some(servers) = doc.get("mcp_servers").and_then(Item::as_array_of_tables) {
        for table in servers {
            let name = table
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let command = table
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = table
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let (env_keys, env_sources) =
                table.get("env").map(extract_env_pairs).unwrap_or_default();
            let available = command_available(&command);
            out.push(McpEntry {
                name,
                command,
                args,
                env_keys,
                env_sources,
                available,
            });
        }
    }
    Ok(out)
}

/// Read `env` from either an inline table (`env = { K = "V" }`) or a standard
/// nested table (`[mcp_servers.env]` / `[env]` under the array-of-tables entry).
fn extract_env_pairs(item: &Item) -> (Vec<String>, Vec<String>) {
    let mut keys = Vec::new();
    let mut sources = Vec::new();
    if let Some(t) = item.as_inline_table() {
        for (k, v) in t.iter() {
            keys.push(k.to_string());
            // Non-string values become empty → doctor treats as invalid ref
            // without echoing anything.
            sources.push(v.as_str().map(str::to_string).unwrap_or_default());
        }
    } else if let Some(t) = item.as_table() {
        for (k, v) in t.iter() {
            keys.push(k.to_string());
            sources.push(
                v.as_value()
                    .and_then(|val| val.as_str())
                    .map(str::to_string)
                    .unwrap_or_default(),
            );
        }
    }
    (keys, sources)
}

/// Add an MCP server. Fails if one with the same name already exists.
///
/// Each `env` pair is `(destination_key, source_ref)`. The source must be an
/// environment variable **name** (or empty / `$NAME`); cleartext secret values
/// are rejected and never written to disk.
pub fn add(
    name: &str,
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<PathBuf, McpConfigError> {
    add_at(&config_path()?, name, command, args, env)
}

/// Add a server to a specific config file.
pub fn add_at(
    path: &std::path::Path,
    name: &str,
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<PathBuf, McpConfigError> {
    let mut doc = document_at(path)?;

    // Ensure the array-of-tables exists.
    if !doc.contains_key("mcp_servers") {
        doc["mcp_servers"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let servers = doc["mcp_servers"]
        .as_array_of_tables_mut()
        .ok_or_else(|| McpConfigError::Parse("`mcp_servers` is not an array of tables".into()))?;

    if servers
        .iter()
        .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
    {
        return Err(McpConfigError::Exists(name.to_string()));
    }

    let mut table = Table::new();
    table["name"] = value(name);
    table["command"] = value(command);
    let mut arg_arr = Array::new();
    for a in args {
        arg_arr.push(a.as_str());
    }
    table["args"] = value(arg_arr);
    if !env.is_empty() {
        let mut env_tab = InlineTable::new();
        for (k, v) in env {
            if !is_env_name_ref(k) {
                return Err(McpConfigError::InvalidKey(k.clone()));
            }
            let source = normalize_env_ref(k, v)?;
            env_tab.insert(k, Value::from(source.as_str()));
        }
        table["env"] = value(env_tab);
    }
    servers.push(table);

    write_document(path, &doc)?;
    Ok(path.to_path_buf())
}

/// Remove an MCP server by name.
pub fn remove(name: &str) -> Result<PathBuf, McpConfigError> {
    remove_at(&config_path()?, name)
}

/// Remove a server from a specific config file.
pub fn remove_at(path: &std::path::Path, name: &str) -> Result<PathBuf, McpConfigError> {
    let mut doc = document_at(path)?;
    let servers = doc
        .get_mut("mcp_servers")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| McpConfigError::NotFound(name.to_string()))?;

    let before = servers.len();
    servers.retain(|t| t.get("name").and_then(|v| v.as_str()) != Some(name));
    if servers.len() == before {
        return Err(McpConfigError::NotFound(name.to_string()));
    }
    write_document(path, &doc)?;
    Ok(path.to_path_buf())
}

fn write_document(path: &std::path::Path, doc: &DocumentMut) -> Result<(), McpConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| McpConfigError::Io(e.to_string()))?;
    }
    std::fs::write(path, doc.to_string()).map_err(|e| McpConfigError::Io(e.to_string()))
}

fn command_available(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }
    if command.contains('/') {
        return std::path::Path::new(command).is_file();
    }
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|d| d.join(command).is_file()))
        .unwrap_or(false)
}

/// Whether `s` is a legal POSIX-ish environment variable name (any case).
pub fn is_env_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `s` is an allowed MCP env **name reference**: `UPPER_SNAKE`
/// (`[A-Z_][A-Z0-9_]*`). Lowercase literals like `password` / `hunter2` are
/// rejected so accidental cleartext cannot be stored under the ref schema.
pub fn is_env_name_ref(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// True when `raw` looks like a cleartext credential rather than an env name.
pub fn looks_like_secret_token(raw: &str) -> bool {
    let s = raw.trim();
    if s.is_empty() {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxs-",
        "xoxe-",
        "aiza",
        "bearer ",
    ];
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // Long mixed-case / digit strings are almost never env names.
    if s.len() >= 24 {
        let has_lower = s.chars().any(|c| c.is_ascii_lowercase());
        let has_digit = s.chars().any(|c| c.is_ascii_digit());
        let pure_upper_snake = s
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !pure_upper_snake && has_lower && has_digit {
            return true;
        }
    }
    false
}

/// Normalize a CLI `--env KEY=VALUE` pair into a source env-var **name**.
///
/// Accepted forms: empty value (→ KEY), `$NAME`, or a bare UPPER_SNAKE name.
/// Cleartext secrets and identifier-shaped literals (`password`) are rejected
/// without being echoed.
pub fn normalize_env_ref(key: &str, raw: &str) -> Result<String, McpConfigError> {
    let trimmed = raw.trim();
    if looks_like_secret_token(trimmed) {
        return Err(McpConfigError::SecretValue {
            key: key.to_string(),
        });
    }
    let name = if trimmed.is_empty() {
        key.to_string()
    } else if let Some(rest) = trimmed.strip_prefix('$') {
        rest.to_string()
    } else {
        trimmed.to_string()
    };
    if looks_like_secret_token(&name) {
        return Err(McpConfigError::SecretValue {
            key: key.to_string(),
        });
    }
    if !is_env_name_ref(&name) {
        // Identifier-shaped but not UPPER_SNAKE (`password`, `hunter2`) is
        // almost always accidental cleartext — reject as SecretValue, no echo.
        // Spaces / punctuation → InvalidSource (same no-echo guarantee).
        if is_env_identifier(&name) {
            return Err(McpConfigError::SecretValue {
                key: key.to_string(),
            });
        }
        return Err(McpConfigError::InvalidSource {
            key: key.to_string(),
        });
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_list_remove_roundtrip_preserves_other_content() {
        let dir = std::env::temp_dir().join(format!("leveler-mcpcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        // Seed a config with an unrelated table + comment.
        std::fs::write(
            &cfg,
            "# my config\ndefault_model = \"deepseek/deepseek-v4-pro\"\n",
        )
        .unwrap();

        add_at(
            &cfg,
            "playwright",
            "npx",
            &["-y".into(), "@playwright/mcp@latest".into()],
            &[],
        )
        .unwrap();
        add_at(
            &cfg,
            "gh",
            "gh-mcp",
            &[],
            &[("GITHUB_TOKEN".into(), "GITHUB_TOKEN".into())],
        )
        .unwrap();

        let servers = list_at(&cfg).unwrap();
        assert_eq!(servers.len(), 2);
        assert!(
            servers
                .iter()
                .any(|s| s.name == "playwright" && s.command == "npx")
        );
        assert!(servers.iter().any(|s| {
            s.name == "gh"
                && s.env_keys == vec!["GITHUB_TOKEN"]
                && s.env_sources == vec!["GITHUB_TOKEN"]
        }));

        // Duplicate rejected.
        assert!(matches!(
            add_at(&cfg, "gh", "x", &[], &[]),
            Err(McpConfigError::Exists(_))
        ));

        // The unrelated content survived the format-preserving edits.
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("# my config"));
        assert!(text.contains("default_model"));

        remove_at(&cfg, "playwright").unwrap();
        assert_eq!(list_at(&cfg).unwrap().len(), 1);
        assert!(matches!(
            remove_at(&cfg, "nope"),
            Err(McpConfigError::NotFound(_))
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_stores_env_name_references_not_cleartext_secrets() {
        let dir = std::env::temp_dir().join(format!("leveler-mcpcfg-sec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");

        // Empty value → self-reference; `$NAME` stripped to name.
        add_at(
            &cfg,
            "svc",
            "echo",
            &[],
            &[
                ("GITHUB_TOKEN".into(), String::new()),
                ("OTHER_KEY".into(), "$MY_SOURCE".into()),
            ],
        )
        .unwrap();

        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("GITHUB_TOKEN"), "key must be stored: {text}");
        assert!(
            text.contains("MY_SOURCE"),
            "source name must be stored: {text}"
        );
        // Must not look like a secret was written.
        assert!(!text.contains("sk-"));
        assert!(!text.contains("ghp_"));

        let servers = list_at(&cfg).unwrap();
        let s = servers.iter().find(|s| s.name == "svc").unwrap();
        assert_eq!(s.env_keys, vec!["GITHUB_TOKEN", "OTHER_KEY"]);
        assert_eq!(s.env_sources, vec!["GITHUB_TOKEN", "MY_SOURCE"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_rejects_secret_shaped_env_values_without_echoing() {
        let dir = std::env::temp_dir().join(format!("leveler-mcpcfg-rej-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");

        let secret = "sk-supersecret-value-do-not-persist";
        let err = add_at(
            &cfg,
            "bad",
            "echo",
            &[],
            &[("API_KEY".into(), secret.into())],
        )
        .unwrap_err();
        assert!(
            matches!(&err, McpConfigError::SecretValue { key } if key == "API_KEY"),
            "got {err:?}"
        );
        assert!(
            !err.to_string().contains(secret),
            "must never echo the secret value"
        );

        // Config file must not have been created with the secret (add failed
        // before write, or write never included it).
        if cfg.exists() {
            let text = std::fs::read_to_string(&cfg).unwrap();
            assert!(
                !text.contains(secret),
                "config must not contain secret: {text}"
            );
        }

        // ghp_ prefix
        let err = add_at(
            &cfg,
            "bad2",
            "echo",
            &[],
            &[(
                "GITHUB_TOKEN".into(),
                "ghp_abcdefghijklmnopqrstuvwxyz0123456789".into(),
            )],
        )
        .unwrap_err();
        assert!(matches!(err, McpConfigError::SecretValue { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalize_env_ref_accepts_name_forms() {
        assert_eq!(normalize_env_ref("TOKEN", "").unwrap(), "TOKEN".to_string());
        assert_eq!(
            normalize_env_ref("TOKEN", "TOKEN").unwrap(),
            "TOKEN".to_string()
        );
        assert_eq!(
            normalize_env_ref("TOKEN", "$OTHER").unwrap(),
            "OTHER".to_string()
        );
        assert!(matches!(
            normalize_env_ref("TOKEN", "sk-abc"),
            Err(McpConfigError::SecretValue { .. })
        ));
        assert!(matches!(
            normalize_env_ref("TOKEN", "not a name"),
            Err(McpConfigError::InvalidSource { .. })
        ));
    }

    #[test]
    fn normalize_env_ref_rejects_identifier_shaped_plaintext() {
        // Avoid substrings of the error template itself (e.g. the word "secret").
        for literal in ["password", "hunter2", "s3cr3tvalue"] {
            let err = normalize_env_ref("API_KEY", literal).unwrap_err();
            assert!(
                matches!(err, McpConfigError::SecretValue { ref key } if key == "API_KEY"),
                "literal {literal:?} must be rejected as SecretValue, got {err:?}"
            );
            assert!(
                !err.to_string().contains(literal),
                "must never echo the literal value in: {}",
                err
            );
        }
        // Mixed / lowercase bare names are not UPPER_SNAKE refs.
        assert!(normalize_env_ref("API_KEY", "GithubToken").is_err());
        assert!(normalize_env_ref("API_KEY", "$other").is_err());
    }

    #[test]
    fn list_at_reads_standard_env_table() {
        let dir = std::env::temp_dir().join(format!("leveler-mcpcfg-tbl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(
            &cfg,
            r#"
[[mcp_servers]]
name = "gh"
command = "gh-mcp"
args = []

[mcp_servers.env]
GITHUB_TOKEN = "GITHUB_TOKEN"
"#,
        )
        .unwrap();
        let servers = list_at(&cfg).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].env_keys, vec!["GITHUB_TOKEN"]);
        assert_eq!(servers[0].env_sources, vec!["GITHUB_TOKEN"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_never_includes_values_in_env_keys() {
        let dir = std::env::temp_dir().join(format!("leveler-mcpcfg-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        // Plant a legacy cleartext secret; list must still only expose keys.
        let secret = "ghp_legacyPlantedSecretValue0001";
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
        let servers = list_at(&cfg).unwrap();
        assert_eq!(servers[0].env_keys, vec!["GITHUB_TOKEN"]);
        // env_sources may still hold the raw string for doctor redaction, but
        // callers that display must use env_keys only (mcp list does).
        assert!(!format!("{:?}", servers[0].env_keys).contains(secret));
        std::fs::remove_dir_all(&dir).ok();
    }
}
