//! The `mcp` and `lsp` subcommands: MCP server configuration and one-shot
//! language-server queries.

use std::path::PathBuf;

use leveler_project::Layout;

use crate::cli::McpCommand;
use crate::output::Line;

fn language_from_ext(file: &std::path::Path) -> Option<leveler_project::Language> {
    use leveler_project::Language::*;
    match file.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(Rust),
        Some("go") => Some(Go),
        Some("py") => Some(Python),
        Some("ts") | Some("tsx") => Some(TypeScript),
        Some("js") | Some("jsx") => Some(JavaScript),
        _ => None,
    }
}

pub(crate) fn cmd_mcp(command: McpCommand) -> anyhow::Result<std::process::ExitCode> {
    use leveler_app::mcp_config;
    match command {
        McpCommand::List => {
            let servers = mcp_config::list().map_err(|e| anyhow::anyhow!("{e}"))?;
            if servers.is_empty() {
                println!("{}", Line::warn("No MCP servers configured."));
                println!("  Add stdio: leveler mcp add <name> -- <command> [args...]");
                println!(
                    "  Add HTTP:  leveler mcp add <name> --url <http(s)://host/mcp> [--header Name=ENV] [--bearer-env ENV]"
                );
                return Ok(std::process::ExitCode::SUCCESS);
            }
            println!(
                "{}",
                Line::heading(&format!("MCP servers ({})", servers.len()))
            );
            for s in servers {
                let mark = if s.available {
                    console::style("✓").green()
                } else {
                    console::style("✗").red()
                };
                if s.url.is_some() {
                    let url = s.url.as_deref().unwrap_or("");
                    println!("  {mark} {}  →  streamable_http {url}", s.name);
                    if !s.header_names.is_empty() {
                        println!("      headers: {}", s.header_names.join(", "));
                    }
                    if let Some(source) = &s.bearer_token_env {
                        println!("      bearer: ${source}");
                    }
                    if !s.available {
                        println!(
                            "      {}",
                            console::style("(URL is not a valid http(s) endpoint)").dim()
                        );
                    }
                } else {
                    let cmd = std::iter::once(s.command.clone())
                        .chain(s.args.clone())
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("  {mark} {}  →  stdio {cmd}", s.name);
                    if !s.env_keys.is_empty() {
                        // Keys only — never print resolved values (secrets-never-persist).
                        println!("      env: {}", s.env_keys.join(", "));
                    }
                    if !s.available {
                        println!(
                            "      {}",
                            console::style("(command not found on PATH)").dim()
                        );
                    }
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        McpCommand::Add {
            name,
            env,
            command,
            url,
            header,
            bearer_env,
        } => {
            let path = if let Some(url) = url {
                if !command.is_empty() || !env.is_empty() {
                    anyhow::bail!(
                        "HTTP MCP uses --url, --header and --bearer-env; do not also pass --env or a command"
                    );
                }
                mcp_config::add_streamable_http(&name, &url, &header, bearer_env.as_deref())
                    .map_err(|e| anyhow::anyhow!("{e}"))?
            } else {
                if !header.is_empty() || bearer_env.is_some() {
                    anyhow::bail!(
                        "stdio MCP uses --env and a command; --header and --bearer-env require --url"
                    );
                }
                let (cmd, args) = command
                    .split_first()
                    .ok_or_else(|| anyhow::anyhow!("no launch command provided after `--`"))?;
                mcp_config::add(&name, cmd, args, &env).map_err(|e| anyhow::anyhow!("{e}"))?
            };
            println!(
                "{}",
                Line::ok(&format!("Added MCP server `{name}` to {}", path.display()))
            );
            println!("  Its tools will be exposed as mcp__{name}__* on the next run.");
            Ok(std::process::ExitCode::SUCCESS)
        }
        McpCommand::Remove { name } => {
            let path = mcp_config::remove(&name).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "{}",
                Line::ok(&format!(
                    "Removed MCP server `{name}` from {}",
                    path.display()
                ))
            );
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

pub(crate) async fn cmd_lsp(
    layout: Layout,
    file: PathBuf,
    diagnostics: bool,
) -> anyhow::Result<std::process::ExitCode> {
    let language = language_from_ext(&file)
        .ok_or_else(|| anyhow::anyhow!("unsupported file type: {}", file.display()))?;
    let spec = leveler_lsp::server_for(language)
        .ok_or_else(|| anyhow::anyhow!("no language server known for {language:?}"))?;
    if !leveler_lsp::server_available(language) {
        anyhow::bail!("language server `{}` is not installed", spec.program);
    }

    let root = layout.require_workspace()?.to_path_buf();
    let path = if file.is_absolute() {
        file.clone()
    } else {
        root.join(&file)
    };

    println!(
        "{}",
        Line::heading(&format!("LSP: {} via {}", file.display(), spec.program))
    );

    let client = leveler_lsp::LspClient::start(&spec.program, &spec.args, &root)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    client
        .open(&path, &spec.language_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let symbols = client
        .document_symbols(&path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "\n{}",
        Line::heading(&format!("Symbols ({})", symbols.len()))
    );
    for s in &symbols {
        let container = s
            .container
            .as_deref()
            .map(|c| format!(" (in {c})"))
            .unwrap_or_default();
        println!("  [{}] {}{container}", s.kind, s.name);
    }

    if diagnostics {
        let diags = client
            .wait_for_diagnostics(&path, std::time::Duration::from_secs(30))
            .await;
        println!(
            "\n{}",
            Line::heading(&format!("Diagnostics ({})", diags.len()))
        );
        for d in &diags {
            println!("  line {}: [{}] {}", d.line + 1, d.severity, d.message);
        }
    }

    client.shutdown().await;
    Ok(std::process::ExitCode::SUCCESS)
}
