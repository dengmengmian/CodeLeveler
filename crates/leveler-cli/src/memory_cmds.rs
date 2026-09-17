//! CLI surface for project-scoped durable memory (WM-04).
//!
//! Store root: [`Layout::memory_dir`]. Agent-side `remember`/`forget` tools still
//! require interactive approval (K36). These subcommands are **user-authoritative**
//! writes — the human is managing memory directly (including pending accept).

use leveler_memory::{MemoryStore, ProposeOutcome, collect_turn_candidates};
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
            // Pending candidates were listed nowhere, so `memory accept <id>`
            // had no way to learn an id — the consent gate was unreachable in
            // practice.
            let pending = store.list_pending().unwrap_or_default();
            if !pending.is_empty() {
                println!();
                println!("{}", Line::heading("Pending (awaiting your consent)"));
                for c in &pending {
                    println!("  [{}] {}", c.id, c.title);
                }
                println!("  adopt with: leveler memory accept <id>");
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
                "  memory_dir={} active={} pending={} archived={}",
                root.display(),
                a,
                pending.len(),
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
        MemoryCommand::Remember {
            title,
            body,
            kind,
            tags,
        } => {
            let saved = remember_direct(&store, &title, &body, kind, tags)?;
            println!(
                "{}",
                Line::ok(&format!("remembered [{}]: {}", saved.id, saved.title))
            );
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Pending => {
            let pending = store.list_pending()?;
            println!("{}", Line::heading("Pending memory candidates"));
            if pending.is_empty() {
                println!("  (none)");
            } else {
                for c in &pending {
                    println!(
                        "  [{}] {}  (key={:?}, source={:?})",
                        c.id, c.title, c.key, c.source
                    );
                    let snippet: String = c.body.chars().take(100).collect();
                    if !snippet.is_empty() {
                        println!("         {snippet}");
                    }
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Accept { id } => match store.accept(&id) {
            Ok(saved) => {
                println!(
                    "{}",
                    Line::ok(&format!("accepted [{}]: {}", saved.id, saved.title))
                );
                Ok(std::process::ExitCode::SUCCESS)
            }
            Err(leveler_memory::MemoryError::NotFound(_)) => {
                println!("{}", Line::warn(&format!("pending not found: {id}")));
                Ok(std::process::ExitCode::FAILURE)
            }
            Err(e) => Err(e.into()),
        },
        MemoryCommand::Reject { id } => match store.reject(&id) {
            Ok(rec) => {
                println!(
                    "{}",
                    Line::ok(&format!(
                        "rejected; suppressed fingerprint={}",
                        rec.fingerprint
                    ))
                );
                Ok(std::process::ExitCode::SUCCESS)
            }
            Err(leveler_memory::MemoryError::NotFound(_)) => {
                println!("{}", Line::warn(&format!("pending not found: {id}")));
                Ok(std::process::ExitCode::FAILURE)
            }
            Err(e) => Err(e.into()),
        },
        MemoryCommand::Propose { text } => {
            // Repository signals are no longer scanned into candidates: a
            // lockfile is read from the working tree when needed, never
            // remembered, so `--scan-pm` had nothing left to do.
            let outcomes = collect_turn_candidates(&store, &text, None)?;
            if outcomes.is_empty() {
                println!("{}", Line::warn("No candidates extracted."));
                return Ok(std::process::ExitCode::SUCCESS);
            }
            println!("{}", Line::heading("Propose results"));
            for o in outcomes {
                match o {
                    ProposeOutcome::Pending(c) => {
                        println!("  pending [{}] {}", c.id, c.title);
                    }
                    ProposeOutcome::Suppressed { fingerprint } => {
                        println!("  suppressed fingerprint={fingerprint}");
                    }
                    ProposeOutcome::AlreadyActive { id } => {
                        println!("  already active id={id}");
                    }
                    ProposeOutcome::AlreadyPending(c) => {
                        println!("  already pending [{}] {}", c.id, c.title);
                    }
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

/// Pure helper used by CLI and unit tests: open store under an explicit root.
#[cfg(test)]
pub(crate) fn memory_round_trip_via_store(root: &std::path::Path) -> anyhow::Result<()> {
    let store = MemoryStore::open(root)?;
    let e = leveler_memory::new_entry(
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

/// A user-authoritative direct write (`leveler memory remember`).
///
/// Uses `remember_deduplicated`, NOT `remember`: the latter is an upsert by id
/// and is reserved for the accept-by-key path, where replacing the previous
/// entry IS the intent. Using it here silently replaced a memory the user had
/// already saved whenever the new title slugged to the same id — which a
/// repeated title always does, and which every CJK-only title does via the
/// timestamp fallback. `base.md` promises the opposite: remember never
/// overwrites, and a second entry competes in recall.
fn remember_direct(
    store: &MemoryStore,
    title: &str,
    body: &str,
    kind: crate::cli::MemoryKindArg,
    tags: Vec<String>,
) -> Result<leveler_memory::MemoryEntry, leveler_memory::MemoryError> {
    use crate::cli::MemoryKindArg;
    use leveler_memory::MemoryKind;
    store.activate(
        title,
        body,
        match kind {
            MemoryKindArg::Preference => MemoryKind::Preference,
            MemoryKindArg::Decision => MemoryKind::Decision,
            MemoryKindArg::Note => MemoryKind::Note,
        },
        tags,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_memory::{ProposeOutcome, parse_inferred_preference};
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

    /// A direct write must never replace a memory the user already saved.
    /// `base.md` promises remember does not overwrite; the CLI used the upsert
    /// path instead, so saving a second note under a title that slugs to the
    /// same id silently destroyed the first one. A CJK-only title hits this
    /// every time: it slugs to empty and falls back to a second-precision
    /// timestamp, so two notes written in the same second collided.
    #[test]
    fn a_direct_write_never_replaces_an_existing_memory() {
        let root =
            std::env::temp_dir().join(format!("leveler-memory-cli-nodup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = MemoryStore::open(&root).unwrap();

        let first = remember_direct(
            &store,
            "Deploy notes",
            "第一版正文",
            crate::cli::MemoryKindArg::Note,
            vec![],
        )
        .unwrap();
        let second = remember_direct(
            &store,
            "Deploy notes",
            "第二版正文完全不同",
            crate::cli::MemoryKindArg::Note,
            vec![],
        )
        .unwrap();
        assert_ne!(first.id, second.id, "a second note needs its own id");
        assert_eq!(store.list_active().unwrap().len(), 2, "neither is lost");
        assert!(
            store
                .read_active(&first.id)
                .unwrap()
                .body
                .contains("第一版"),
            "the first body survived"
        );

        // Two CJK-only titles in the same second both slug to empty and share
        // the timestamp fallback; both must still land.
        let a = remember_direct(
            &store,
            "状态色约定",
            "映射表实现",
            crate::cli::MemoryKindArg::Note,
            vec![],
        )
        .unwrap();
        let b = remember_direct(
            &store,
            "搜索框过滤范围",
            "只按 payer 过滤",
            crate::cli::MemoryKindArg::Note,
            vec![],
        )
        .unwrap();
        assert_ne!(a.id, b.id, "same-second CJK titles must not collide");
        assert_eq!(store.list_active().unwrap().len(), 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// User-authoritative accept path used by `leveler memory accept` (not agent AutoApprove).
    #[test]
    fn user_accept_path_promotes_pending_only_on_explicit_accept() {
        let root =
            std::env::temp_dir().join(format!("leveler-memory-cli-accept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = MemoryStore::open(&root).unwrap();
        let c = parse_inferred_preference("我通常希望 prefer gated memory writes").unwrap();
        let pending = match store.propose(c).unwrap() {
            ProposeOutcome::Pending(c) => c,
            other => panic!("{other:?}"),
        };
        assert_eq!(store.list_active().unwrap().len(), 0);
        store.accept(&pending.id).unwrap();
        assert_eq!(store.list_active().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
