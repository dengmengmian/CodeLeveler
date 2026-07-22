//! CLI surface for durable permission rules (SEC-1).
//!
//! Rules live in:
//! - global: `$LEVELER_HOME/permissions.yaml` (default `~/.leveler/permissions.yaml`)
//! - project: `~/.leveler/projects/<hash>/permissions.yaml` (written by
//!   ApproveAlways, next to `sessions.db`)
//! - repo: `<repo>/.leveler/permissions.yaml` (user-authored / legacy
//!   ApproveAlways target, still honored)
//!
//! `list` shows all three; `clear` removes the project file and the repo file.

use std::path::{Path, PathBuf};

use leveler_execution::{
    PermissionRule, RuleEffect, clear_rules_file, load_rules_file, project_rules_path,
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

            println!("{}", Line::heading("Permission rules"));
            println!();
            print_source("global", &global_path);
            print_source("project", &layout.permissions_path());
            print_source("repo", &project_rules_path(&layout.repo_root));
            Ok(std::process::ExitCode::SUCCESS)
        }
        PermissionsCommand::Clear => {
            let mut cleared = false;
            for path in [
                layout.permissions_path(),
                project_rules_path(&layout.repo_root),
            ] {
                let existed = path.is_file();
                clear_rules_file(&path).map_err(anyhow::Error::msg)?;
                if existed {
                    cleared = true;
                    println!(
                        "{}",
                        Line::ok(&format!("cleared project rules ({})", path.display()))
                    );
                }
            }
            if !cleared {
                println!("{}", Line::warn("no project rules file to clear"));
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
