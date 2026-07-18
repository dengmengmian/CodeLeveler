//! CLI surface for project-scoped durable memory (WM-04).
//!
//! Store root: [`Layout::memory_dir`]. Agent-side `remember`/`forget` tools still
//! require interactive approval (K36). These subcommands are **user-authoritative**
//! writes — the human is managing memory directly.

use leveler_memory::{MemoryStore, new_entry};
use leveler_project::Layout;

use crate::cli::MemoryCommand;
use crate::output::Line;

pub(crate) fn cmd_memory(
    layout: Layout,
    cmd: MemoryCommand,
) -> anyhow::Result<std::process::ExitCode> {
    let root = layout.memory_dir();
    let store = MemoryStore::open(&root)?;
    match cmd {
        MemoryCommand::List { archived } => {
            println!("{}", Line::heading("Active memory"));
            let active = store.list_active()?;
            if active.is_empty() {
                println!("  (none)");
            } else {
                for e in &active {
                    println!("  [{}] {}", e.id, e.title);
                }
            }
            if archived {
                println!();
                println!("{}", Line::heading("Archived memory"));
                let arch = store.list_archived()?;
                if arch.is_empty() {
                    println!("  (none)");
                } else {
                    for e in &arch {
                        println!("  [{}] {}", e.id, e.title);
                    }
                }
            }
            let (a, b) = store.counts()?;
            println!();
            println!(
                "  memory_dir={} active={} archived={}",
                root.display(),
                a,
                b
            );
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Search { query, limit } => {
            let hits = store.search(&query, limit)?;
            if hits.is_empty() {
                println!("{}", Line::warn("No matches."));
            } else {
                println!("{}", Line::heading("Search results"));
                for (e, score) in hits {
                    println!("  [{:.3}] [{}] {}", score, e.id, e.title);
                    let snippet: String = e.body.chars().take(120).collect();
                    if !snippet.is_empty() {
                        println!("         {snippet}");
                    }
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Show { id } => match store.read_active(&id) {
            Ok(e) => {
                println!("{}", Line::heading(&format!("Memory [{}]", e.id)));
                println!("  title: {}", e.title);
                println!("  created: {}", e.created_at);
                println!("  updated: {}", e.updated_at);
                if !e.tags.is_empty() {
                    println!("  tags: {}", e.tags.join(", "));
                }
                println!();
                println!("{}", e.body);
                Ok(std::process::ExitCode::SUCCESS)
            }
            Err(leveler_memory::MemoryError::NotFound(_)) => {
                println!("{}", Line::warn(&format!("not found: {id}")));
                Ok(std::process::ExitCode::FAILURE)
            }
            Err(e) => Err(e.into()),
        },
        MemoryCommand::Forget { id } => {
            let e = store.forget(&id)?;
            println!("{}", Line::ok(&format!("archived [{}]: {}", e.id, e.title)));
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Remember { title, body, tags } => {
            let entry = new_entry(&title, &body, tags);
            let saved = store.remember(entry)?;
            println!(
                "{}",
                Line::ok(&format!("remembered [{}]: {}", saved.id, saved.title))
            );
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

/// Pure helper used by CLI and unit tests: open store under an explicit root.
#[cfg(test)]
pub(crate) fn memory_round_trip_via_store(root: &std::path::Path) -> anyhow::Result<()> {
    let store = MemoryStore::open(root)?;
    let e = new_entry(
        "use-workspace-write-default",
        "Prefer WorkspaceWrite for routine edits.",
        vec!["preference".into()],
    );
    store.remember(e.clone())?;
    let hits = store.search("WorkspaceWrite", 5)?;
    anyhow::ensure!(hits.len() == 1, "expected one search hit");
    anyhow::ensure!(hits[0].0.id == e.id);
    let shown = store.read_active(&e.id)?;
    anyhow::ensure!(shown.body.contains("WorkspaceWrite"));
    store.forget(&e.id)?;
    anyhow::ensure!(store.search("WorkspaceWrite", 5)?.is_empty());
    anyhow::ensure!(store.counts()? == (0, 1));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn remember_search_show_forget_round_trip() {
        let root =
            std::env::temp_dir().join(format!("leveler-memory-cli-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        memory_round_trip_via_store(&root).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        // Ensure Layout::memory_dir is the path CLI will open for a real repo.
        let layout = Layout::resolve(PathBuf::from("/tmp/leveler-mem-layout-check"), None);
        let mem = layout.memory_dir();
        assert!(
            mem.to_string_lossy().contains("memory")
                || mem.file_name().and_then(|s| s.to_str()) == Some("memory"),
            "memory_dir should end with memory: {}",
            mem.display()
        );
    }
}
