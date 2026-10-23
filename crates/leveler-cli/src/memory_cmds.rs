//! CLI surface for project-scoped durable memory (WM-04).
//!
//! Store root: [`Layout::memory_dir`]. Agent-side `remember`/`forget` tools still
//! require interactive approval (K36). These subcommands are **user-authoritative**
//! writes — the human is managing memory directly (including pending accept).

use leveler_memory::{
    CandidateKind, CandidateSource, MemoryCandidate, MemoryEntry, MemoryKind, MemoryStore,
    ProposeOutcome, is_sensitive, is_sensitive_candidate, redact_candidate, title_from_body,
};
use leveler_project::Layout;

use crate::cli::MemoryCommand;
use crate::output::Line;

/// A stored title is safe to print only when the record is not credential-shaped.
fn safe_title(entry: &MemoryEntry) -> &str {
    if is_sensitive(entry) {
        leveler_memory::REDACTED_TITLE
    } else {
        &entry.title
    }
}

/// A short marker so a person can tell an evidence-bound lesson from a
/// user-stated fact in a one-line listing. The label is the standing the
/// evidence supports, not the stored flag: an observation-only constraint is
/// `已观察`, never `已验证`.
fn experience_suffix(entry: &MemoryEntry) -> String {
    match entry.experience.as_ref() {
        None => String::new(),
        Some(experience) => format!(
            " [经验·{}]",
            match experience.standing() {
                leveler_memory::ExperienceStanding::VerifiedResolution => "已验证",
                leveler_memory::ExperienceStanding::Observed => "已观察",
                leveler_memory::ExperienceStanding::Inferred => "推断",
            }
        ),
    }
}

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
                    println!("  [{}] {}{}", e.id, safe_title(e), experience_suffix(e));
                    if is_sensitive(e) {
                        println!("         sensitive: content withheld (leveler memory audit)");
                    }
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
                    if is_sensitive_candidate(c) {
                        let mut redacted = c.clone();
                        redact_candidate(&mut redacted);
                        println!("  [{}] {}", redacted.id, redacted.title);
                        println!("         sensitive: content withheld (leveler memory audit)");
                    } else {
                        let marker = if c.experience.is_some() {
                            " [经验]"
                        } else {
                            ""
                        };
                        println!("  [{}] {}{}", c.id, c.title, marker);
                    }
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
                        println!("  [{}] {}{}", e.id, e.title, experience_suffix(e));
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
        MemoryCommand::Status => {
            // The switch is project configuration, the same authority the
            // runtime reads. A missing config means the built-in defaults.
            let config = layout
                .primary_workspace()
                .and_then(leveler_project::ProjectConfig::load)
                .unwrap_or_default();
            let total = config.memory.enabled;
            let user = total && config.memory.user_enabled;
            let experience = total && config.memory.experience_enabled;
            let on = |enabled: bool| if enabled { "on" } else { "off" };
            let active = store.list_active()?;
            let pending = store.list_pending()?;
            let archived = store.list_archived()?;
            let experience_active = active
                .iter()
                .filter(|entry| MemoryKind::of(entry) == MemoryKind::Experience)
                .count();
            let experience_pending = pending
                .iter()
                .filter(|candidate| candidate.experience.is_some())
                .count();
            let experience_archived = archived
                .iter()
                .filter(|entry| MemoryKind::of(entry) == MemoryKind::Experience)
                .count();
            let experimental = if experience { " (experimental)" } else { "" };
            println!("{}", Line::heading("Memory switches"));
            println!("  total: {}", on(total));
            println!(
                "  user facts: {} (active {} · pending {} · archived {})",
                on(user),
                active.len().saturating_sub(experience_active),
                pending.len().saturating_sub(experience_pending),
                archived.len().saturating_sub(experience_archived),
            );
            println!(
                "  auto experience: {}{} (active {} · pending {} · archived {})",
                on(experience),
                experimental,
                experience_active,
                experience_pending,
                experience_archived,
            );
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Search { query, limit } => {
            // Search the full active set before filtering, so sensitive hits
            // cannot occupy the requested window and hide safe matches.
            let active_count = store.list_active()?.len();
            let hits = store.search(&query, active_count.max(limit))?;
            let withheld = hits.iter().filter(|(entry, _)| is_sensitive(entry)).count();
            let hits: Vec<_> = hits
                .into_iter()
                .filter(|(entry, _)| !is_sensitive(entry))
                .collect();
            if hits.is_empty() {
                println!("{}", Line::warn("No matches."));
            } else {
                println!("{}", Line::heading("Search results"));
                for (e, score) in hits {
                    println!(
                        "  [{:.3}] [{}] {}{}",
                        score,
                        e.id,
                        e.title,
                        experience_suffix(&e)
                    );
                    let snippet: String = e.body.chars().take(120).collect();
                    if !snippet.is_empty() {
                        println!("         {snippet}");
                    }
                }
            }
            if withheld > 0 {
                println!("  {withheld} sensitive match(es) withheld; run `leveler memory audit`");
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Show { id, archived } => {
            let entry = if archived {
                store
                    .list_archived()?
                    .into_iter()
                    .find(|entry| entry.id == id)
            } else {
                match store.read_active(&id) {
                    Ok(entry) => Some(entry),
                    Err(leveler_memory::MemoryError::NotFound(_)) => None,
                    Err(e) => return Err(e.into()),
                }
            };
            match entry {
                Some(e) => {
                    let sensitive = is_sensitive(&e);
                    println!("{}", Line::heading(&format!("Memory [{}]", e.id)));
                    println!("  title: {}", safe_title(&e));
                    println!("  created: {}", e.created_at);
                    println!("  updated: {}", e.updated_at);
                    if archived {
                        println!("  status: {:?}", e.effective_status());
                        if let Some(by) = &e.superseded_by {
                            println!("  superseded_by: {by}");
                        }
                        if let Some(at) = &e.superseded_at {
                            println!("  superseded_at: {at}");
                        }
                    }
                    if sensitive {
                        println!("  sensitive: content withheld; run `leveler memory audit`");
                    } else {
                        if !e.tags.is_empty() {
                            println!("  tags: {}", e.tags.join(", "));
                        }
                        if let Some(experience) = e.experience.as_ref() {
                            println!(
                                "  experience: category={} verification={} standing={}",
                                experience.category.as_str(),
                                match experience.verification {
                                    leveler_memory::ExperienceVerification::Verified => "verified",
                                    leveler_memory::ExperienceVerification::Unverified => {
                                        "unverified"
                                    }
                                },
                                experience.standing().as_str()
                            );
                            println!("  applies_when: {}", experience.applies_when);
                            if let Some(invalid_if) = experience.invalid_if.as_deref() {
                                println!("  invalid_if: {invalid_if}");
                            }
                            for evidence in &experience.evidence {
                                println!("  evidence[{}]: {}", evidence.kind, evidence.reference);
                            }
                        }
                        println!();
                        println!("{}", e.body);
                    }
                    Ok(std::process::ExitCode::SUCCESS)
                }
                None => {
                    println!(
                        "{}",
                        Line::warn(&format!(
                            "not found{}: {id}",
                            if archived { " in archive" } else { "" }
                        ))
                    );
                    Ok(std::process::ExitCode::FAILURE)
                }
            }
        }
        MemoryCommand::Forget { id } => {
            let e = store.forget(&id)?;
            println!(
                "{}",
                Line::ok(&format!("archived [{}]: {}", e.id, safe_title(&e)))
            );
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Restore { id } => match store.restore(&id) {
            Ok(entry) => {
                println!(
                    "{}",
                    Line::ok(&format!("restored [{}]: {}", entry.id, safe_title(&entry)))
                );
                Ok(std::process::ExitCode::SUCCESS)
            }
            Err(leveler_memory::MemoryError::NotFound(_)) => {
                println!("{}", Line::warn(&format!("not found: {id}")));
                Ok(std::process::ExitCode::FAILURE)
            }
            Err(leveler_memory::MemoryError::Invalid(reason)) => {
                println!("{}", Line::warn(&reason));
                Ok(std::process::ExitCode::FAILURE)
            }
            Err(e) => Err(e.into()),
        },
        MemoryCommand::Remember {
            title,
            body,
            kind,
            tags,
        } => {
            let saved = remember_direct(&store, &title, &body, kind, tags)?;
            let message = match saved.supersedes.as_deref() {
                Some(old) => format!(
                    "updated [{}]: {} (superseded [{}])",
                    saved.id, saved.title, old
                ),
                None => format!("remembered [{}]: {}", saved.id, saved.title),
            };
            println!("{}", Line::ok(&message));
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Pending => {
            let pending = store.list_pending()?;
            println!("{}", Line::heading("Pending memory candidates"));
            if pending.is_empty() {
                println!("  (none)");
            } else {
                for c in &pending {
                    let sensitive = is_sensitive_candidate(c);
                    if sensitive {
                        let mut redacted = c.clone();
                        redact_candidate(&mut redacted);
                        println!(
                            "  [{}] {}  (key={:?}, source={:?})",
                            redacted.id, redacted.title, redacted.key, redacted.source
                        );
                        println!("         sensitive: content withheld (leveler memory audit)");
                    } else {
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
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Accept { id } => match store.accept(&id) {
            Ok(saved) => {
                println!(
                    "{}",
                    Line::ok(&format!("accepted [{}]: {}", saved.id, safe_title(&saved)))
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
        MemoryCommand::Audit => {
            let findings = store.audit_sensitive()?;
            println!("{}", Line::heading("Sensitive memory audit"));
            if findings.is_empty() {
                println!("  (none)");
            } else {
                for finding in &findings {
                    println!(
                        "  [{:?}] [{}] fields={}",
                        finding.location,
                        finding.id,
                        finding.fields.join(",")
                    );
                }
                println!();
                println!(
                    "  {} record(s) need redaction; run `leveler memory scrub --apply`",
                    findings.len()
                );
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::AuditIdentity => {
            let audit = store.audit_identity()?;
            println!(
                "{}",
                Line::heading("Memory identity audit (dry run; no files are modified)")
            );
            println!(
                "  total={} ready={} needs_confirmation={} possible_duplicate={} \
                 possibly_stale={} sensitive={}",
                audit.summary.total,
                audit.summary.ready,
                audit.summary.needs_confirmation,
                audit.summary.possible_duplicate,
                audit.summary.possibly_stale,
                audit.summary.sensitive
            );
            for record in &audit.records {
                println!(
                    "  [{:?}] {} [{}] {}",
                    record.location,
                    record.state.as_str(),
                    record.id,
                    record.title
                );
                if let Some(key) = &record.key {
                    println!("         key={key}");
                }
                if !record.related.is_empty() {
                    println!("         related={}", record.related.join(","));
                }
                println!("         {}", record.reason);
            }
            println!();
            println!("  dry run only: this command does not rewrite user memory.");
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Scrub { id, apply } => {
            let report = store.scrub_sensitive(id.as_deref(), apply)?;
            if report.findings == 0 {
                println!("{}", Line::ok("No sensitive memory records found."));
            } else if apply {
                println!(
                    "{}",
                    Line::ok(&format!("Redacted {} record(s).", report.redacted))
                );
            } else {
                println!(
                    "{}",
                    Line::warn(&format!(
                        "Dry run: {} record(s) would be redacted; pass --apply to write.",
                        report.findings
                    ))
                );
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Propose { text } => {
            // The CLI has no model call and no structured subject here. Store
            // the text as a pending proposal verbatim; a person still accepts
            // it, so nothing becomes current truth on the strength of a rule.
            let candidate = MemoryCandidate::new(
                title_from_body(&text),
                &text,
                CandidateKind::Free,
                None,
                CandidateSource::SystemPropose,
                vec!["manual".to_string()],
            )?;
            let outcomes = vec![store.propose(candidate)?];
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
/// timestamp fallback. Ordinary notes never overwrite: a second note competes
/// in recall. A body that states a durable assignment ("以后…固定为 X") is an
/// update of that semantic identity; the lifecycle keeps the old value as
/// superseded history.
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
    use leveler_memory::ProposeOutcome;
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
        let c = leveler_memory::MemoryCandidate::new(
            "gated memory writes",
            "prefer gated memory writes",
            leveler_memory::CandidateKind::Preference,
            None,
            leveler_memory::CandidateSource::SystemInferred,
            vec![],
        )
        .unwrap();
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
