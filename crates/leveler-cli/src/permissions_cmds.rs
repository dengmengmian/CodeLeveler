//! CLI surface for durable permission rules (SEC-1).
//!
//! Rules live in:
//! - global: `$LEVELER_HOME/permissions.yaml` (default `~/.leveler/permissions.yaml`)
//! - project: `<repo>/.leveler/permissions.yaml` (written by ApproveAlways)
//!
//! `list` shows both; `clear` only removes the project file.

use std::path::{Path, PathBuf};

use leveler_execution::{
    PermissionRule, RuleEffect, clear_project_rules, load_rules_file, project_rules_path,
};
use leveler_project::Layout;

use crate::cli::PermissionsCommand;
use crate::output::Line;

pub(crate) fn cmd_permissions(
    layout: Layout,
    cmd: PermissionsCommand,
) -> anyhow::Result<std::process::ExitCode> {
    match cmd {
        PermissionsCommand::List => {
            let global_home = leveler_home();
            let global_path = global_home.join("permissions.yaml");
            let project_path = project_rules_path(&layout.repo_root);

            println!("{}", Line::heading("Permission rules"));
            println!();
            print_source("global", &global_path);
            print_source("project", &project_path);
            Ok(std::process::ExitCode::SUCCESS)
        }
        PermissionsCommand::Clear => {
            let path = project_rules_path(&layout.repo_root);
            let existed = path.is_file();
            clear_project_rules(&layout.repo_root).map_err(anyhow::Error::msg)?;
            if existed {
                println!(
                    "{}",
                    Line::ok(&format!("cleared project rules ({})", path.display()))
                );
            } else {
                println!(
                    "{}",
                    Line::warn(&format!(
                        "no project rules file to clear ({})",
                        path.display()
                    ))
                );
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

fn print_source(label: &str, path: &Path) {
    println!("{}  {}", Line::heading(label), path.display());
    match load_rules_file(path) {
        Ok(set) if set.is_empty() => {
            if path.is_file() {
                println!("  (file empty — no rules)");
            } else {
                println!("  (no file)");
            }
        }
        Ok(set) => {
            for (i, rule) in set.rules().iter().enumerate() {
                println!("  {}. {}", i + 1, format_rule(rule));
            }
        }
        Err(e) => {
            println!("  {}", Line::fail(&e));
        }
    }
    println!();
}

fn format_rule(rule: &PermissionRule) -> String {
    let effect = match rule.effect {
        RuleEffect::Allow => "allow",
        RuleEffect::Ask => "ask",
        RuleEffect::Deny => "deny",
    };
    let mut parts = Vec::new();
    if let Some(tool) = &rule.match_.tool {
        parts.push(format!("tool={tool}"));
    }
    if let Some(prefix) = &rule.match_.command_prefix {
        parts.push(format!("command_prefix={prefix:?}"));
    }
    if let Some(glob) = &rule.match_.path_glob {
        parts.push(format!("path_glob={glob:?}"));
    }
    if parts.is_empty() {
        parts.push("(empty match)".into());
    }
    format!("{effect}  {}", parts.join(" "))
}

/// The leveler home, with a local `.leveler` fallback (should not hit in normal
/// interactive use). Home-resolution order is shared via
/// [`leveler_core::leveler_home_dir_from`].
fn leveler_home() -> PathBuf {
    leveler_core::leveler_home_dir_from(|k| std::env::var_os(k))
        .unwrap_or_else(|| PathBuf::from(".leveler"))
}

// Unit tests for this module live in `tests/permissions.rs` (bin-only crate).
