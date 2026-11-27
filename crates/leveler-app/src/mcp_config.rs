//! Manage MCP servers in the resident global config (`~/.leveler/config.toml`)
//! without hand-editing TOML — the `mcp add/list/remove` flow.
//! Edits are format-preserving (comments and unrelated tables survive).
//!
//! MCP `env` entries store **environment variable name references only**
//! (never cleartext secret values). At runtime the app resolves each reference
//! from the process environment.

use std::path::{Path, PathBuf};

use fs2::FileExt;
use leveler_client_protocol::mcp::{McpServerSpec, McpTransport};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use crate::global_config::GlobalConfig;

/// One configured MCP server, as shown by `list`.
#[derive(Debug, Clone)]
pub struct McpEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// `stdio` or `streamable_http`.
    pub transport: String,
    /// Streamable HTTP endpoint. `None` for stdio.
    pub url: Option<String>,
    /// Whether the runtime should connect this server. Absence in the file
    /// means enabled; a disabled entry stays listed and editable.
    pub enabled: bool,
    /// Destination env keys forwarded into the MCP process (display only).
    pub env_keys: Vec<String>,
    /// Source env var names corresponding to [`Self::env_keys`] (same order).
    /// Doctor checks these; values are never displayed.
    pub env_sources: Vec<String>,
    /// Configured request-header names. Values are never displayed.
    pub header_names: Vec<String>,
    /// Environment variable names those headers resolve from (same order).
    pub header_sources: Vec<String>,
    /// Environment variable holding the bearer token, if configured.
    pub bearer_token_env: Option<String>,
    /// Stdio: `command` is on `PATH`. HTTP: the URL is a valid http(s) endpoint.
    /// This is not a live connection probe.
    pub available: bool,
}

impl McpEntry {
    /// A display-safe endpoint: the launch command or the HTTP URL. Arguments
    /// and header values are deliberately not shown.
    pub fn endpoint_label(&self) -> String {
        if let Some(url) = &self.url {
            url.clone()
        } else {
            self.command.clone()
        }
    }
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
    #[error("invalid MCP server name: {0}")]
    InvalidName(String),
    #[error("a stdio MCP server requires a command")]
    MissingCommand,
    #[error("a streamable HTTP MCP server requires a URL")]
    MissingUrl,
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
    #[error("set Authorization with --bearer-env, not --header")]
    AuthorizationHeader,
    #[error("{0}")]
    InvalidUrl(String),
    #[error(
        "stdio MCP servers take --env and a command; HTTP servers take --url, --header and --bearer-env"
    )]
    MixedTransport,
    #[error("io error: {0}")]
    Io(String),
    #[error("the configuration kept changing while it was being updated; nothing was written")]
    ConcurrentModification,
}

/// The resident config path (`$LEVELER_HOME/config.toml` or `~/.leveler/config.toml`).
pub fn config_path() -> Result<PathBuf, McpConfigError> {
    GlobalConfig::path().ok_or(McpConfigError::NoPath)
}

fn read_text(path: &Path) -> Result<String, McpConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(McpConfigError::Io(format!("{}: {error}", path.display()))),
    }
}

fn parse_text(text: &str) -> Result<DocumentMut, McpConfigError> {
    text.parse::<DocumentMut>().map_err(|error| {
        McpConfigError::Parse(crate::global_config::safe_toml_parse_error(
            &text,
            error.message(),
            error.span(),
        ))
    })
}

fn document_at(path: &Path) -> Result<DocumentMut, McpConfigError> {
    parse_text(&read_text(path)?)
}

/// How many read-compute-write rounds one edit may take before a still-changing
/// file is reported as a conflict instead of overwriting someone else's write.
const MAX_CONFIG_WRITE_ATTEMPTS: usize = 8;

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".lock");
    path.with_file_name(name)
}

/// Read-edit-write the global config under a process-shared advisory lock.
///
/// MCP management can be reached from several Desktop windows, the TUI and the
/// CLI at once. The exclusive lock serializes them; the content check also
/// catches a writer that does not take this lock (the provider editor and the
/// first-run/login writer), so an edit computed from an older snapshot is
/// recomputed instead of replacing the newer file.
fn edit_document_at<T>(
    path: &Path,
    mut apply: impl FnMut(&mut DocumentMut) -> Result<T, McpConfigError>,
) -> Result<T, McpConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| McpConfigError::Io(format!("{}: {error}", parent.display())))?;
        leveler_core::private_fs::restrict_dir(parent)
            .map_err(|error| McpConfigError::Io(error.to_string()))?;
    }
    let lock_path = lock_path(path);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| McpConfigError::Io(format!("{}: {error}", lock_path.display())))?;
    lock.lock_exclusive()
        .map_err(|error| McpConfigError::Io(format!("{}: {error}", lock_path.display())))?;
    let result = (|| {
        for _ in 0..MAX_CONFIG_WRITE_ATTEMPTS {
            let text = read_text(path)?;
            let mut doc = parse_text(&text)?;
            let out = apply(&mut doc)?;
            if read_text(path)? != text {
                continue;
            }
            write_document(path, &doc)?;
            return Ok(out);
        }
        Err(McpConfigError::ConcurrentModification)
    })();
    let _ = FileExt::unlock(&lock);
    result
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
            let url = table
                .get("url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let transport = table
                .get("transport")
                .and_then(|v| v.as_str())
                .unwrap_or(if url.is_some() {
                    "streamable_http"
                } else {
                    "stdio"
                })
                .to_string();
            let (env_keys, env_sources) =
                table.get("env").map(extract_env_pairs).unwrap_or_default();
            let (header_names, header_sources) = table
                .get("headers")
                .map(extract_env_pairs)
                .unwrap_or_default();
            let bearer_token_env = table
                .get("bearer_token_env")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let enabled = table
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let available = if url.is_some() {
                url.as_deref().is_some_and(|url| {
                    leveler_tools::mcp::validate_streamable_http_url(url).is_ok()
                })
            } else {
                command_available(&command)
            };
            out.push(McpEntry {
                name,
                command,
                args,
                transport,
                url,
                enabled,
                env_keys,
                env_sources,
                header_names,
                header_sources,
                bearer_token_env,
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
    edit_document_at(path, |doc| {
        // Ensure the array-of-tables exists.
        if !doc.contains_key("mcp_servers") {
            doc["mcp_servers"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        let servers = doc["mcp_servers"].as_array_of_tables_mut().ok_or_else(|| {
            McpConfigError::Parse("`mcp_servers` is not an array of tables".into())
        })?;

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
        Ok(())
    })?;
    Ok(path.to_path_buf())
}

/// Add a Streamable HTTP MCP server. Header values and `bearer_env` are
/// environment variable **names**. The URL is stored as given after it passes
/// the http(s) check; credentials in the URL are rejected.
pub fn add_streamable_http(
    name: &str,
    url: &str,
    headers: &[(String, String)],
    bearer_env: Option<&str>,
) -> Result<PathBuf, McpConfigError> {
    add_streamable_http_at(&config_path()?, name, url, headers, bearer_env)
}

/// [`add_streamable_http`] against an explicit config file.
pub fn add_streamable_http_at(
    path: &std::path::Path,
    name: &str,
    url: &str,
    headers: &[(String, String)],
    bearer_env: Option<&str>,
) -> Result<PathBuf, McpConfigError> {
    leveler_tools::mcp::validate_streamable_http_url(url).map_err(McpConfigError::InvalidUrl)?;
    edit_document_at(path, |doc| {
        if !doc.contains_key("mcp_servers") {
            doc["mcp_servers"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        let servers = doc["mcp_servers"].as_array_of_tables_mut().ok_or_else(|| {
            McpConfigError::Parse("`mcp_servers` is not an array of tables".into())
        })?;
        if servers
            .iter()
            .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
        {
            return Err(McpConfigError::Exists(name.to_string()));
        }
        let mut table = Table::new();
        table["name"] = value(name);
        table["transport"] = value("streamable_http");
        table["url"] = value(url);
        if !headers.is_empty() {
            let mut header_tab = InlineTable::new();
            for (key, raw) in headers {
                if key.eq_ignore_ascii_case("authorization") {
                    return Err(McpConfigError::AuthorizationHeader);
                }
                if !is_header_name(key) {
                    return Err(McpConfigError::InvalidKey(key.clone()));
                }
                let source = normalize_env_ref(key, raw)?;
                header_tab.insert(key, Value::from(source.as_str()));
            }
            table["headers"] = value(header_tab);
        }
        if let Some(raw) = bearer_env {
            let source = normalize_env_ref("bearer_token_env", raw)?;
            table["bearer_token_env"] = value(source.as_str());
        }
        servers.push(table);
        Ok(())
    })?;
    Ok(path.to_path_buf())
}

fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Remove an MCP server by name.
pub fn remove(name: &str) -> Result<PathBuf, McpConfigError> {
    remove_at(&config_path()?, name)
}

/// Remove a server from a specific config file.
pub fn remove_at(path: &std::path::Path, name: &str) -> Result<PathBuf, McpConfigError> {
    edit_document_at(path, |doc| {
        let servers = doc
            .get_mut("mcp_servers")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| McpConfigError::NotFound(name.to_string()))?;

        let before = servers.len();
        servers.retain(|t| t.get("name").and_then(|v| v.as_str()) != Some(name));
        if servers.len() == before {
            return Err(McpConfigError::NotFound(name.to_string()));
        }
        Ok(())
    })?;
    Ok(path.to_path_buf())
}

fn write_document(path: &std::path::Path, doc: &DocumentMut) -> Result<(), McpConfigError> {
    // The shared atomic writer stages a uniquely named sibling and renames it;
    // a reader (config watcher, doctor, another client) never sees a half file.
    crate::global_config::write_global_config_atomic(path, &doc.to_string())
        .map_err(|error| McpConfigError::Io(error.to_string()))
}

/// A server name a runtime and a client can both address unambiguously.
///
/// Conservative on purpose: an identifier that survives a CLI argument, a
/// display row and the model-facing `mcp__<server>__<tool>` prefix without
/// quoting.
pub fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a server name is required".into());
    }
    if name.len() > 64 {
        return Err("a server name is limited to 64 bytes".into());
    }
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Err("a server name must start with a letter or digit".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("a server name may contain only letters, digits, `-`, `_` and `.`".into());
    }
    Ok(())
}

/// The runtime's one Streamable HTTP URL rule, exposed so a client boundary
/// can refuse an invalid endpoint before forwarding it.
pub fn validate_streamable_http_url(url: &str) -> Result<(), String> {
    leveler_tools::mcp::validate_streamable_http_url(url)
}

/// Build one `[[mcp_servers]]` table from a protocol spec.
///
/// This is the editor's own validation boundary: the runtime service also
/// validates, but a config write must never depend on a caller having done it.
fn spec_table(spec: &McpServerSpec) -> Result<Table, McpConfigError> {
    validate_name(&spec.name).map_err(McpConfigError::InvalidName)?;
    let mut table = Table::new();
    table["name"] = value(spec.name.trim());
    match spec.transport {
        McpTransport::Stdio => {
            let command = spec.command.as_deref().unwrap_or("").trim();
            if command.is_empty() {
                return Err(McpConfigError::MissingCommand);
            }
            if spec
                .url
                .as_deref()
                .is_some_and(|url| !url.trim().is_empty())
                || !spec.headers.is_empty()
                || spec.bearer_token_env.is_some()
            {
                return Err(McpConfigError::MixedTransport);
            }
            table["command"] = value(command);
            let mut arg_arr = Array::new();
            for arg in &spec.args {
                arg_arr.push(arg.as_str());
            }
            table["args"] = value(arg_arr);
            if !spec.env.is_empty() {
                let mut env_tab = InlineTable::new();
                for reference in &spec.env {
                    if !is_env_identifier(&reference.key) {
                        return Err(McpConfigError::InvalidKey(reference.key.clone()));
                    }
                    let source = normalize_env_ref(&reference.key, &reference.source)?;
                    env_tab.insert(&reference.key, Value::from(source.as_str()));
                }
                table["env"] = value(env_tab);
            }
        }
        McpTransport::StreamableHttp => {
            let url = spec.url.as_deref().unwrap_or("").trim();
            if url.is_empty() {
                return Err(McpConfigError::MissingUrl);
            }
            leveler_tools::mcp::validate_streamable_http_url(url)
                .map_err(McpConfigError::InvalidUrl)?;
            if spec
                .command
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty())
                || !spec.args.is_empty()
                || !spec.env.is_empty()
            {
                return Err(McpConfigError::MixedTransport);
            }
            table["transport"] = value("streamable_http");
            table["url"] = value(url);
            if !spec.headers.is_empty() {
                let mut header_tab = InlineTable::new();
                for reference in &spec.headers {
                    if reference.key.eq_ignore_ascii_case("authorization") {
                        return Err(McpConfigError::AuthorizationHeader);
                    }
                    if !is_header_name(&reference.key) {
                        return Err(McpConfigError::InvalidKey(reference.key.clone()));
                    }
                    let source = normalize_env_ref(&reference.key, &reference.source)?;
                    header_tab.insert(&reference.key, Value::from(source.as_str()));
                }
                table["headers"] = value(header_tab);
            }
            if let Some(raw) = &spec.bearer_token_env {
                let source = normalize_env_ref("bearer_token_env", raw)?;
                table["bearer_token_env"] = value(source.as_str());
            }
        }
    }
    if !spec.enabled {
        table["enabled"] = value(false);
    }
    Ok(table)
}

/// Add one server from a protocol spec.
pub fn add_spec_at(
    path: &std::path::Path,
    spec: &McpServerSpec,
) -> Result<PathBuf, McpConfigError> {
    edit_document_at(path, |doc| {
        let table = spec_table(spec)?;
        if !doc.contains_key("mcp_servers") {
            doc["mcp_servers"] = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        let servers = doc["mcp_servers"].as_array_of_tables_mut().ok_or_else(|| {
            McpConfigError::Parse("`mcp_servers` is not an array of tables".into())
        })?;
        if servers
            .iter()
            .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(spec.name.trim()))
        {
            return Err(McpConfigError::Exists(spec.name.clone()));
        }
        servers.push(table);
        Ok(())
    })?;
    Ok(path.to_path_buf())
}

/// Replace the server named `original_name` in place.
pub fn update_spec_at(
    path: &std::path::Path,
    original_name: &str,
    spec: &McpServerSpec,
) -> Result<PathBuf, McpConfigError> {
    edit_document_at(path, |doc| {
        let table = spec_table(spec)?;
        let servers = doc
            .get_mut("mcp_servers")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| McpConfigError::NotFound(original_name.to_string()))?;
        let index = servers
            .iter()
            .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(original_name))
            .ok_or_else(|| McpConfigError::NotFound(original_name.to_string()))?;
        if spec.name.trim() != original_name
            && servers
                .iter()
                .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(spec.name.trim()))
        {
            return Err(McpConfigError::Exists(spec.name.clone()));
        }
        if let Some(slot) = servers.get_mut(index) {
            *slot = table;
        }
        Ok(())
    })?;
    Ok(path.to_path_buf())
}

/// Enable or disable one server without touching its other fields.
///
/// Enabled is the absence of `enabled = false`, which keeps a hand-written
/// config clean and matches the parser's default.
pub fn set_enabled_at(
    path: &std::path::Path,
    name: &str,
    enabled: bool,
) -> Result<PathBuf, McpConfigError> {
    edit_document_at(path, |doc| {
        let servers = doc
            .get_mut("mcp_servers")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| McpConfigError::NotFound(name.to_string()))?;
        let table = servers
            .iter_mut()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
            .ok_or_else(|| McpConfigError::NotFound(name.to_string()))?;
        if enabled {
            table.remove("enabled");
        } else {
            table.insert("enabled", value(false));
        }
        Ok(())
    })?;
    Ok(path.to_path_buf())
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
    fn a_malformed_config_error_does_not_echo_a_secret_value() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-mcpcfg-parse-secret-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        let secret = "sk-e2e-malformed-config-secret";
        // The value is on the broken line. A parser that quotes the source
        // line would put it into the error a client shows.
        std::fs::write(
            &cfg,
            format!("[[mcp_servers]]\nname = \"broken\"\nenv = {{ API_KEY = \"{secret}\" \n"),
        )
        .unwrap();
        let error = list_at(&cfg).expect_err("the config is malformed");
        assert!(
            !error.to_string().contains(secret),
            "parse error leaked the value: {error}"
        );
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
    fn add_streamable_http_stores_refs_not_secret_values() {
        let dir = std::env::temp_dir().join(format!("leveler-mcpcfg-http-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        add_streamable_http_at(
            &cfg,
            "devorder",
            "http://127.0.0.1:8081/mcp",
            &[("X-API-Key".into(), "DEVORDER_MCP_API_KEY".into())],
            Some("DEVORDER_MCP_TOKEN"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("streamable_http"));
        assert!(text.contains("DEVORDER_MCP_API_KEY"));
        assert!(!text.contains("sk-"));
        let server = list_at(&cfg).unwrap().pop().unwrap();
        assert_eq!(server.transport, "streamable_http");
        assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:8081/mcp"));
        assert_eq!(server.header_names, vec!["X-API-Key"]);
        assert_eq!(server.header_sources, vec!["DEVORDER_MCP_API_KEY"]);
        assert_eq!(
            server.bearer_token_env.as_deref(),
            Some("DEVORDER_MCP_TOKEN")
        );
        assert!(server.available);
        let secret = "sk-do-not-write-this-http-key";
        let err = add_streamable_http_at(
            &cfg,
            "other",
            "https://example.com/mcp",
            &[("X-API-Key".into(), secret.into())],
            None,
        )
        .unwrap_err();
        assert!(!err.to_string().contains(secret));
        assert!(
            !std::fs::read_to_string(&cfg).unwrap().contains(secret),
            "rejected header secret must not be written"
        );
        std::fs::remove_dir_all(&dir).ok();
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

    fn spec_dir(tag: &str) -> (PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "leveler-mcpcfg-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        (dir, cfg)
    }

    fn stdio_spec(name: &str) -> McpServerSpec {
        McpServerSpec {
            name: name.into(),
            transport: McpTransport::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "server".into()],
            env: vec![leveler_client_protocol::mcp::McpNameRef {
                key: "GITHUB_TOKEN".into(),
                source: "GITHUB_TOKEN".into(),
            }],
            url: None,
            headers: Vec::new(),
            bearer_token_env: None,
            enabled: true,
        }
    }

    #[test]
    fn a_spec_adds_lists_and_survives_a_disable_roundtrip() {
        let (dir, cfg) = spec_dir("spec");
        add_spec_at(&cfg, &stdio_spec("fs")).unwrap();
        let listed = list_at(&cfg).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "fs");
        assert!(listed[0].enabled);
        assert_eq!(listed[0].env_keys, vec!["GITHUB_TOKEN"]);
        assert_eq!(listed[0].env_sources, vec!["GITHUB_TOKEN"]);

        set_enabled_at(&cfg, "fs", false).unwrap();
        assert!(!list_at(&cfg).unwrap()[0].enabled);
        set_enabled_at(&cfg, "fs", true).unwrap();
        assert!(list_at(&cfg).unwrap()[0].enabled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_replaces_in_place_and_refuses_a_name_collision() {
        let (dir, cfg) = spec_dir("update");
        add_spec_at(&cfg, &stdio_spec("fs")).unwrap();
        add_spec_at(&cfg, &stdio_spec("other")).unwrap();
        let mut renamed = stdio_spec("fs-two");
        renamed.command = Some("node".into());
        update_spec_at(&cfg, "fs", &renamed).unwrap();
        let listed = list_at(&cfg).unwrap();
        assert_eq!(listed[0].name, "fs-two");
        assert_eq!(listed[0].command, "node");
        assert_eq!(listed[1].name, "other");

        let collision = update_spec_at(&cfg, "fs-two", &stdio_spec("other"));
        assert!(matches!(collision, Err(McpConfigError::Exists(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_mixed_or_unnamed_spec_is_refused_before_it_is_written() {
        let (dir, cfg) = spec_dir("refuse");
        let mut mixed = stdio_spec("fs");
        mixed.url = Some("http://localhost:3000/mcp".into());
        assert!(matches!(
            add_spec_at(&cfg, &mixed),
            Err(McpConfigError::MixedTransport)
        ));
        let mut unnamed = stdio_spec("../escape");
        unnamed.name = "../escape".into();
        assert!(matches!(
            add_spec_at(&cfg, &unnamed),
            Err(McpConfigError::InvalidName(_))
        ));
        assert!(!cfg.exists(), "a refused spec never creates the file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_http_spec_keeps_header_sources_and_refuses_authorization() {
        let (dir, cfg) = spec_dir("http");
        let mut spec = McpServerSpec {
            name: "remote".into(),
            transport: McpTransport::StreamableHttp,
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            url: Some("https://example.com/mcp".into()),
            headers: vec![leveler_client_protocol::mcp::McpNameRef {
                key: "X-API-Key".into(),
                source: "SERVICE_API_KEY".into(),
            }],
            bearer_token_env: Some("SERVICE_TOKEN".into()),
            enabled: true,
        };
        add_spec_at(&cfg, &spec).unwrap();
        let listed = list_at(&cfg).unwrap();
        assert_eq!(listed[0].transport, "streamable_http");
        assert_eq!(listed[0].header_names, vec!["X-API-Key"]);
        assert_eq!(listed[0].header_sources, vec!["SERVICE_API_KEY"]);
        assert_eq!(listed[0].bearer_token_env.as_deref(), Some("SERVICE_TOKEN"));

        spec.name = "authorized".into();
        spec.headers = vec![leveler_client_protocol::mcp::McpNameRef {
            key: "Authorization".into(),
            source: "SERVICE_TOKEN".into(),
        }];
        assert!(matches!(
            add_spec_at(&cfg, &spec),
            Err(McpConfigError::AuthorizationHeader)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_mcp_edits_do_not_lose_a_server() {
        let (dir, cfg) = spec_dir("concurrent");
        std::thread::scope(|scope| {
            for index in 0..8 {
                let cfg = cfg.clone();
                scope.spawn(move || {
                    add_spec_at(&cfg, &stdio_spec(&format!("server-{index}"))).unwrap();
                });
            }
        });
        let listed = list_at(&cfg).unwrap();
        assert_eq!(
            listed.len(),
            8,
            "every concurrent edit survived: {listed:?}"
        );
        let names: std::collections::BTreeSet<_> =
            listed.into_iter().map(|entry| entry.name).collect();
        for index in 0..8 {
            assert!(names.contains(&format!("server-{index}")));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_mcp_writer_waits_for_the_configuration_lock() {
        let (dir, cfg) = spec_dir("lock");
        let path = lock_path(&cfg);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let writer_path = cfg.clone();
        let writer = std::thread::spawn(move || {
            let result = add_spec_at(&writer_path, &stdio_spec("waited"));
            done.send(result.is_ok()).unwrap();
        });
        assert!(
            finished
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the writer must not edit the file while another process holds the lock"
        );
        FileExt::unlock(&lock).unwrap();
        assert!(
            finished
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(),
            "the writer proceeds after the lock is released"
        );
        writer.join().unwrap();
        assert_eq!(list_at(&cfg).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
