//! CLI surface for project-scoped durable memory (WM-04).
//!
//! Every read and write goes through the unified [`MemoryService`], and the
//! Markdown store is the only implementation behind it. This command has no
//! second store to touch.
//!
//! This is a plain local process, so it cannot mint a user actor: a model with
//! shell access can run the same argv. Writes therefore run as
//! [`Actor::External`]; the service may queue them as pending or reject them,
//! but it never turns a CLI argument into user authority. Trusted user review
//! (accept/reject/restore and direct activation) is available only from the
//! TUI/desktop, where the command handler mints `UserInvocationId` in process.
//!
//! `diagnose` inspects a Markdown scope without changing it.

use std::path::Path;

use leveler_memory::{
    MemoryLanes, MemoryService, MemoryView, RememberRequest, ReviewRequest, ServiceError,
    ServiceScope, WriteOutcome,
    v2::{Actor, DiagnosticReport, DiagnosticSeverity, MarkdownStore, ScopeRoots},
};
use leveler_project::Layout;

use crate::cli::{MemoryCommand, MemoryKindArg, MemoryScopeArg};
use crate::output::Line;

fn external_actor() -> Actor {
    Actor::External
}

/// Whether a listed record belongs to an enabled lane.
fn lane_allows(lanes: MemoryLanes, view: &MemoryView) -> bool {
    if view.is_experience() {
        lanes.experience
    } else {
        lanes.user
    }
}

/// A stored title is safe to print only when the record is not credential-shaped.
fn view_title(view: &MemoryView) -> &str {
    if view.sensitive {
        leveler_memory::REDACTED_TITLE
    } else {
        &view.title
    }
}

/// A short marker so a person can tell an evidence-bound lesson from a
/// user-stated fact in a one-line listing. The label is the standing the
/// evidence supports, not the stored flag: an observation-only constraint is
/// `已观察`, never `已验证`.
fn experience_suffix(view: &MemoryView) -> String {
    match view.experience.as_ref() {
        None => String::new(),
        Some(experience) => format!(
            " [经验·{}]",
            match experience.standing.as_str() {
                "verified" => "已验证",
                "observed" => "已观察",
                _ => "推断",
            }
        ),
    }
}

/// A per-invocation operation id for a CLI write.
///
/// A repeated command is a new human intent, so this is deliberately not a
/// content digest: saving the same note twice must not silently replay the
/// first write.
fn cli_op_id(verb: &str) -> String {
    format!(
        "cli:{verb}:{}:{}",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    )
}

fn kind_label(kind: MemoryKindArg) -> &'static str {
    match kind {
        MemoryKindArg::Preference => "preference",
        MemoryKindArg::Decision => "decision",
        MemoryKindArg::Note => "note",
    }
}

/// Print a write result and say whether the command achieved its goal.
fn report_outcome(outcome: WriteOutcome) -> std::process::ExitCode {
    match outcome {
        WriteOutcome::Applied {
            id,
            title,
            operation,
            revision,
            superseded,
            replayed,
            ..
        } => {
            let mut detail = String::new();
            if let Some(revision) = revision {
                detail.push_str(&format!(" revision={revision}"));
            }
            if replayed {
                detail.push_str(" (replayed)");
            }
            if !superseded.is_empty() {
                detail.push_str(&format!(" (superseded {})", superseded.join(", ")));
            }
            println!(
                "{}",
                Line::ok(&format!("[{id}] {title}: {operation}{detail}"))
            );
            std::process::ExitCode::SUCCESS
        }
        WriteOutcome::Pending { id, title, reason } => {
            println!(
                "{}",
                Line::ok(&format!("queued for review [{id}]: {title} ({reason})"))
            );
            println!("  review it with /memory accept <id> from the TUI/desktop");
            std::process::ExitCode::SUCCESS
        }
        WriteOutcome::Skipped { existing, reason } => {
            println!(
                "{}",
                Line::ok(&format!("no change: {reason} (existing [{existing}])"))
            );
            std::process::ExitCode::SUCCESS
        }
        WriteOutcome::Conflict {
            target,
            current_revision,
            expected_revision,
        } => {
            println!(
                "{}",
                Line::warn(&format!(
                    "conflict on {target}: current revision {current_revision}, expected {expected_revision}"
                ))
            );
            std::process::ExitCode::FAILURE
        }
        WriteOutcome::Rejected { reason } => {
            println!("{}", Line::warn(&format!("not stored: {reason}")));
            std::process::ExitCode::FAILURE
        }
        WriteOutcome::NotStored { reason } => {
            println!("{}", Line::warn(&format!("not stored: {reason}")));
            std::process::ExitCode::FAILURE
        }
        WriteOutcome::Forgotten { id, title } => {
            // A forget is content destruction: the body, its history and its
            // index rows are gone and only a tombstone remains. Saying
            // "archived" here would tell the user the opposite of what
            // happened.
            println!(
                "{}",
                Line::ok(&format!("forgotten for good [{id}]: {title}"))
            );
            std::process::ExitCode::SUCCESS
        }
    }
}

fn print_view(view: &MemoryView) {
    println!("{}", Line::heading(&format!("Memory [{}]", view.id)));
    println!("  title: {}", view_title(view));
    println!("  status: {}", view.status);
    println!("  kind: {}", view.kind);
    println!("  authority: {}", view.authority);
    if let Some(revision) = view.revision {
        println!("  revision: {revision}");
    }
    if view.pinned_by_user {
        println!("  pinned: user");
    }
    if view.sensitive {
        println!("  sensitive: content withheld");
        return;
    }
    if !view.tags.is_empty() {
        println!("  tags: {}", view.tags.join(", "));
    }
    if let Some(subject) = &view.subject_key {
        println!("  subject_key: {subject}");
    }
    if let Some(value) = &view.value {
        println!("  value: {value}");
    }
    if let Some(expires) = &view.expires_at {
        println!("  expires_at: {expires}");
    }
    if let Some(experience) = &view.experience {
        println!(
            "  experience: standing={} applies_when={}",
            experience.standing, experience.applies_when
        );
        if let Some(invalid_if) = &experience.invalid_if {
            println!("  invalid_if: {invalid_if}");
        }
        for (kind, reference) in &experience.evidence {
            println!("  evidence[{kind}]: {reference}");
        }
    }
    println!();
    println!("{}", view.body);
}

fn pending_not_readable() {
    println!(
        "{}",
        Line::warn(
            "pending review state is only visible from the TUI/desktop; a local process cannot read it (this is not an empty result)"
        )
    );
}

fn review_pending(
    service: &MemoryService,
    actor: &Actor,
    scope: ServiceScope,
    id: &str,
    decision: ReviewRequest,
) -> anyhow::Result<std::process::ExitCode> {
    match service.review(actor, scope, id, decision) {
        Ok(outcome) => Ok(report_outcome(outcome)),
        Err(ServiceError::NeedsReview(_)) => {
            println!(
                "{}",
                Line::warn(
                    "only the TUI/desktop can accept or reject pending memory; a local process cannot claim user authority"
                )
            );
            Ok(std::process::ExitCode::FAILURE)
        }
        Err(ServiceError::NotFound(_)) => {
            println!("{}", Line::warn(&format!("pending not found: {id}")));
            Ok(std::process::ExitCode::FAILURE)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn cmd_memory(
    layout: Layout,
    cmd: MemoryCommand,
) -> anyhow::Result<std::process::ExitCode> {
    // Must run before any store is opened: `diagnose` resolves paths without
    // creating them, and inspecting a broken scope is only useful if it
    // changes nothing.
    if let MemoryCommand::Diagnose {
        current,
        state,
        scope,
    } = &cmd
    {
        return cmd_memory_diagnose(&layout, current, state, *scope);
    }
    if let MemoryCommand::PurgeLegacy { apply } = cmd {
        return cmd_memory_purge_legacy(&layout, apply);
    }

    let config = layout
        .primary_workspace()
        .and_then(leveler_project::ProjectConfig::load)
        .unwrap_or_default();
    let lanes = leveler_app::MemoryPolicy::from_project(&config).lanes();
    let service = leveler_app::memory_service(&layout);
    let actor = external_actor();
    let scope = ServiceScope::Project;

    match cmd {
        MemoryCommand::List { archived } => {
            println!("{}", Line::heading("Active memory"));
            let active = service.listing_for_user(scope, lanes)?;
            if active.is_empty() {
                println!("  (none)");
            } else {
                for view in &active {
                    println!(
                        "  [{}] {}{}",
                        view.id,
                        view_title(view),
                        experience_suffix(view)
                    );
                    if view.sensitive {
                        println!("         sensitive: content withheld");
                    }
                }
            }
            match service.pending(&actor, scope) {
                Ok(pending) => {
                    let pending: Vec<_> = pending
                        .into_iter()
                        .filter(|view| lane_allows(lanes, view))
                        .collect();
                    if !pending.is_empty() {
                        println!();
                        println!("{}", Line::heading("Pending (awaiting review)"));
                        for view in &pending {
                            println!(
                                "  [{}] {}{}",
                                view.id,
                                view_title(view),
                                experience_suffix(view)
                            );
                            if view.sensitive {
                                println!("         sensitive: content withheld");
                            }
                        }
                        println!("  review with: /memory accept <id> from the TUI/desktop");
                    }
                }
                Err(ServiceError::NeedsReview(_)) => pending_not_readable(),
                Err(error) => return Err(error.into()),
            }
            if archived {
                println!();
                println!("{}", Line::heading("Archived memory"));
                let archived = service.archived(&actor, scope, lanes, None)?;
                if archived.is_empty() {
                    println!("  (none)");
                } else {
                    for view in &archived {
                        println!(
                            "  [{}] {}{}",
                            view.id,
                            view_title(view),
                            experience_suffix(view)
                        );
                    }
                }
            }
            let (active_count, archived_count) = service.counts(scope)?;
            println!();
            let current = layout
                .markdown_memory_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(no workspace)".to_string());
            println!("  current_dir={current}");
            println!("  active={active_count} archived={archived_count}");
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Status => {
            // The switch is project configuration, the same authority the
            // runtime reads. A missing config means the built-in defaults.
            let total = config.memory.enabled;
            let user = total && config.memory.user_enabled;
            let experience = total && config.memory.experience_enabled;
            let on = |enabled: bool| if enabled { "on" } else { "off" };
            let active = service.listing_for_user(scope, lanes)?;
            let archived = service.archived(&actor, scope, lanes, None)?;
            let experience_active = active.iter().filter(|view| view.is_experience()).count();
            let experience_archived = archived.iter().filter(|view| view.is_experience()).count();
            let experimental = if experience { " (experimental)" } else { "" };
            // A local process cannot read the pending queue, which is the same
            // rule that makes it unable to approve one. Reporting `pending 0`
            // would then be a claim about a store this process did not read,
            // so the lane lines carry only what was read and the limitation is
            // stated once.
            let pending_readable = service.pending(&actor, scope).is_ok();
            let (user_pending, experience_pending) = match service.pending(&actor, scope) {
                Ok(pending) => {
                    let experience = pending.iter().filter(|view| view.is_experience()).count();
                    (pending.len() - experience, experience)
                }
                _ => (0, 0),
            };
            let lane_counts = |active: usize, pending: usize, archived: usize| {
                if pending_readable {
                    format!("active {active} · pending {pending} · archived {archived}")
                } else {
                    format!("active {active} · archived {archived}")
                }
            };
            println!("{}", Line::heading("Memory switches"));
            println!("  total: {}", on(total));
            println!(
                "  user facts: {} ({})",
                on(user),
                lane_counts(
                    active.len().saturating_sub(experience_active),
                    user_pending,
                    archived.len().saturating_sub(experience_archived),
                ),
            );
            println!(
                "  auto experience: {}{} ({})",
                on(experience),
                experimental,
                lane_counts(experience_active, experience_pending, experience_archived),
            );
            if !pending_readable {
                println!(
                    "  pending review: only visible from the TUI/desktop; a local process cannot read it (this is not zero)"
                );
            }
            let current = layout
                .markdown_memory_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(no workspace)".to_string());
            println!("  store: markdown current_dir={current}");
            if let Some(notice) = config.memory.deprecated_backend() {
                println!("  {}", Line::warn(&notice));
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Search { query, limit } => {
            let hits = service.search(&actor, scope, lanes, &query, limit)?;
            if hits.is_empty() {
                println!("{}", Line::warn("No matches."));
            } else {
                println!("{}", Line::heading("Search results"));
                for (view, score) in hits {
                    println!(
                        "  [{score:.3}] [{}] {}{}",
                        view.id,
                        view_title(&view),
                        experience_suffix(&view)
                    );
                    if view.sensitive {
                        continue;
                    }
                    let snippet: String = view.body.chars().take(120).collect();
                    if !snippet.is_empty() {
                        println!("         {snippet}");
                    }
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Show { id, archived } => {
            if archived {
                let found = service
                    .archived(&actor, scope, lanes, None)?
                    .into_iter()
                    .find(|view| view.id == id);
                return match found {
                    Some(view) => {
                        print_view(&view);
                        Ok(std::process::ExitCode::SUCCESS)
                    }
                    None => {
                        println!("{}", Line::warn(&format!("not found in archive: {id}")));
                        Ok(std::process::ExitCode::FAILURE)
                    }
                };
            }
            let found = service
                .listing_for_user(scope, lanes)?
                .into_iter()
                .find(|view| view.id == id);
            return match found {
                Some(view) => {
                    print_view(&view);
                    Ok(std::process::ExitCode::SUCCESS)
                }
                None => match service.read(&actor, scope, &id) {
                    Ok(view) => {
                        print_view(&view);
                        Ok(std::process::ExitCode::SUCCESS)
                    }
                    Err(ServiceError::NotFound(_)) => {
                        println!("{}", Line::warn(&format!("not found: {id}")));
                        Ok(std::process::ExitCode::FAILURE)
                    }
                    Err(ServiceError::NotCurrent(_)) => {
                        println!(
                            "{}",
                            Line::warn(&format!("not current: {id}; read it with --archived"))
                        );
                        Ok(std::process::ExitCode::FAILURE)
                    }
                    Err(error) => Err(error.into()),
                },
            };
        }
        MemoryCommand::Forget { id } => {
            // A credential-shaped title must stay withheld even as the record
            // is archived; the service already refuses legacy forgets of such
            // records.
            let sensitive = service
                .listing_for_user(scope, lanes)
                .ok()
                .and_then(|views| views.into_iter().find(|view| view.id == id))
                .is_some_and(|view| view.sensitive);
            match service.forget(&actor, scope, &id) {
                Ok(WriteOutcome::Forgotten { id, title }) => {
                    let title = if sensitive {
                        leveler_memory::REDACTED_TITLE
                    } else {
                        title.as_str()
                    };
                    println!(
                        "{}",
                        Line::ok(&format!("forgotten for good [{id}]: {title}"))
                    );
                    Ok(std::process::ExitCode::SUCCESS)
                }
                Ok(outcome) => Ok(report_outcome(outcome)),
                Err(ServiceError::NotFound(_)) => {
                    println!("{}", Line::warn(&format!("not found: {id}")));
                    Ok(std::process::ExitCode::FAILURE)
                }
                Err(ServiceError::NotCurrent(_)) => {
                    println!(
                        "{}",
                        Line::warn(&format!("not current: {id}; nothing to forget"))
                    );
                    Ok(std::process::ExitCode::FAILURE)
                }
                Err(ServiceError::Invalid(reason)) => {
                    println!("{}", Line::warn(&reason));
                    Ok(std::process::ExitCode::FAILURE)
                }
                Err(error) => Err(error.into()),
            }
        }
        MemoryCommand::Remember {
            title,
            body,
            kind,
            tags,
        } => {
            // A local process may only propose: the service decides whether
            // that becomes pending or is rejected, never user authority.
            let mut request =
                RememberRequest::new(cli_op_id("remember"), scope, title, body, kind_label(kind));
            request.tags = tags;
            match service.propose(&actor, request) {
                Ok(WriteOutcome::Rejected { reason }) => {
                    println!("{}", Line::warn(&format!("not stored: {reason}")));
                    println!(
                        "  a local process cannot claim user authority; use /remember in the TUI/desktop"
                    );
                    Ok(std::process::ExitCode::FAILURE)
                }
                Ok(outcome) => Ok(report_outcome(outcome)),
                Err(ServiceError::Invalid(reason)) => {
                    println!("{}", Line::warn(&reason));
                    Ok(std::process::ExitCode::FAILURE)
                }
                Err(error) => Err(error.into()),
            }
        }
        MemoryCommand::Pending => {
            let pending = match service.pending(&actor, scope) {
                Ok(pending) => pending,
                Err(ServiceError::NeedsReview(_)) => {
                    pending_not_readable();
                    return Ok(std::process::ExitCode::FAILURE);
                }
                Err(error) => return Err(error.into()),
            };
            println!("{}", Line::heading("Pending memory candidates"));
            let pending: Vec<_> = pending
                .into_iter()
                .filter(|view| lane_allows(lanes, view))
                .collect();
            if pending.is_empty() {
                println!("  (none)");
            } else {
                for view in &pending {
                    println!(
                        "  [{}] {}{}  (authority={})",
                        view.id,
                        view_title(view),
                        experience_suffix(view),
                        view.authority
                    );
                    if view.sensitive {
                        println!("         sensitive: content withheld");
                        continue;
                    }
                    let snippet: String = view.body.chars().take(100).collect();
                    if !snippet.is_empty() {
                        println!("         {snippet}");
                    }
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Accept { id } => {
            review_pending(&service, &actor, scope, &id, ReviewRequest::Accept)
        }
        MemoryCommand::Reject { id } => review_pending(
            &service,
            &actor,
            scope,
            &id,
            ReviewRequest::Reject {
                reason: "rejected from a local process".into(),
            },
        ),
        MemoryCommand::Propose { text } => {
            let mut request = RememberRequest::new(
                cli_op_id("propose"),
                scope,
                leveler_memory::title_from_body(&text),
                text,
                "note",
            );
            request.tags = vec!["manual".to_string()];
            match service.propose(&actor, request) {
                Ok(WriteOutcome::Rejected { reason }) => {
                    println!("{}", Line::warn(&format!("not stored: {reason}")));
                    println!("  a local process cannot claim user authority; use the TUI/desktop");
                    Ok(std::process::ExitCode::FAILURE)
                }
                Ok(outcome) => Ok(report_outcome(outcome)),
                Err(ServiceError::Invalid(reason)) => {
                    println!("{}", Line::warn(&reason));
                    Ok(std::process::ExitCode::FAILURE)
                }
                Err(error) => Err(error.into()),
            }
        }
        MemoryCommand::PurgeLegacy { .. } | MemoryCommand::Diagnose { .. } => {
            unreachable!("these are handled before any store is opened")
        }
    }
}

/// Remove the retired JSON memory stores under this machine's state root.
///
/// Dry run by default. The report names every store it would delete and every
/// candidate it refuses, so a blocked entry is a visible decision instead of a
/// silent skip.
fn cmd_memory_purge_legacy(layout: &Layout, apply: bool) -> anyhow::Result<std::process::ExitCode> {
    let state_root = layout.home().state_dir();
    let report = leveler_project::purge(&state_root, apply)?;
    println!("{}", Line::heading("Retired JSON memory stores"));
    println!("  state_root: {}", state_root.display());
    if report.stores.is_empty() {
        println!("  {}", Line::ok("no legacy store found"));
        return Ok(std::process::ExitCode::SUCCESS);
    }
    for store in &report.stores {
        match &store.verdict {
            leveler_project::StoreVerdict::LegacyStore { files, bytes } => {
                let action = if store.removed {
                    Line::ok("removed")
                } else {
                    "would remove".to_string()
                };
                println!(
                    "  {action} {} ({files} file(s), {bytes} bytes)",
                    store.path.display()
                );
            }
            leveler_project::StoreVerdict::Blocked { reason } => {
                println!("  {} {}", Line::warn("BLOCKED"), store.path.display());
                println!("      {reason}");
            }
        }
    }
    let blocked = report.blocked().len();
    println!();
    if apply {
        println!(
            "  removed {} store(s), {} file(s), {} bytes",
            report.removed_stores(),
            report.removed_files(),
            report.removed_bytes()
        );
    } else {
        println!("  dry run: pass --apply to delete the stores listed above");
    }
    if blocked > 0 {
        println!(
            "  {}",
            Line::warn(&format!(
                "{blocked} candidate(s) were left alone; inspect them by hand"
            ))
        );
        return Ok(std::process::ExitCode::FAILURE);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Read-only report for a Markdown (Memory 2.0) scope.
///
/// Nothing here creates, recovers or repairs: the store's `diagnose` resolves
/// paths without `prepare` and only reads. A blocking finding exits non-zero so
/// a script can tell "broken" from "clean", never from "empty".
fn cmd_memory_diagnose(
    layout: &Layout,
    current: &Path,
    state: &Path,
    scope: MemoryScopeArg,
) -> anyhow::Result<std::process::ExitCode> {
    let mut roots = ScopeRoots::new(scope.into(), current, state);
    if let Some(repo) = layout.primary_workspace() {
        roots = roots.with_repo_root(repo);
    }
    let report = MarkdownStore::diagnose(roots)?;
    print_diagnostic_report(&report);
    Ok(if report.is_blocking() {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    })
}

fn print_diagnostic_report(report: &DiagnosticReport) {
    println!("{}", Line::heading("Markdown memory diagnostics"));
    println!("  scope: {:?}", report.scope);
    println!("  current: {}", report.current_root.display());
    println!("  state: {}", report.state_root.display());
    if report.initialized {
        println!(
            "  trusted active: {}  pending: {}",
            report.active_records, report.pending_records
        );
    } else {
        println!(
            "  {}",
            Line::warn(
                "not initialized (current or state root is missing); this report created nothing"
            )
        );
    }
    if report.issues.is_empty() {
        println!("  {}", Line::ok("no issues"));
        return;
    }
    for issue in &report.issues {
        let label = match issue.severity {
            DiagnosticSeverity::Blocking => Line::fail("BLOCKING"),
            DiagnosticSeverity::ActionRequired => Line::warn("ACTION REQUIRED"),
            DiagnosticSeverity::Warning => "warning".to_string(),
        };
        match &issue.path {
            Some(path) => println!("  {label} {:?} {}", issue.kind, path.display()),
            None => println!("  {label} {:?}", issue.kind),
        }
        println!("      {}", issue.detail);
        println!("      fix: {}", issue.suggestion);
    }
}

/// A project-scope service rooted under a test directory.
#[cfg(test)]
fn test_service(root: &std::path::Path) -> MemoryService {
    MemoryService::new(leveler_memory::MemoryRoots::new().with_project(
        leveler_memory::v2::ScopeRoots::new(
            leveler_memory::v2::MemoryScope::Project,
            root.join("memory"),
            root.join("state"),
        ),
    ))
}

#[cfg(test)]
fn test_user() -> Actor {
    Actor::User {
        invocation: leveler_memory::v2::UserInvocationId::mint(format!(
            "cli-test-{}",
            std::process::id()
        )),
    }
}

/// The round trip the command surface relies on: remember, search, show,
/// forget.
#[cfg(test)]
pub(crate) fn memory_round_trip_via_service(root: &std::path::Path) -> anyhow::Result<()> {
    let service = test_service(root);
    let id = service
        .remember(
            &test_user(),
            RememberRequest::new(
                cli_op_id("round-trip"),
                ServiceScope::Project,
                "use-workspace-write-default",
                "Prefer WorkspaceWrite for routine edits.",
                "preference",
            ),
        )?
        .applied_id()
        .expect("the write is current")
        .to_string();
    let hits = service.search(
        &external_actor(),
        ServiceScope::Project,
        MemoryLanes::ALL,
        "WorkspaceWrite",
        5,
    )?;
    anyhow::ensure!(hits.len() == 1, "expected one search hit");
    anyhow::ensure!(hits[0].0.id == id);
    let shown = service.read(&external_actor(), ServiceScope::Project, &id)?;
    anyhow::ensure!(shown.body.contains("WorkspaceWrite"));
    service.forget(&test_user(), ServiceScope::Project, &id)?;
    anyhow::ensure!(
        service
            .search(
                &external_actor(),
                ServiceScope::Project,
                MemoryLanes::ALL,
                "WorkspaceWrite",
                5
            )?
            .is_empty()
    );
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
        memory_round_trip_via_service(&root).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        // The CLI addresses the project's Markdown current root.
        let layout = Layout::resolve(PathBuf::from("/tmp/leveler-mem-layout-check"), None);
        let mem = layout
            .markdown_memory_dir()
            .expect("a workspace has a project memory root");
        assert!(
            mem.to_string_lossy().contains("memory")
                || mem.file_name().and_then(|s| s.to_str()) == Some("memory"),
            "the project memory root should end with memory: {}",
            mem.display()
        );
    }

    /// A direct write must never replace a memory the user already saved, and
    /// two CJK-only titles written in the same second must not collide.
    #[test]
    fn a_direct_write_never_replaces_an_existing_memory() {
        let root =
            std::env::temp_dir().join(format!("leveler-memory-cli-nodup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let service = test_service(&root);
        let remember = |op: &str, title: &str, body: &str| {
            service
                .remember(
                    &test_user(),
                    RememberRequest::new(cli_op_id(op), ServiceScope::Project, title, body, "note"),
                )
                .unwrap()
                .applied_id()
                .expect("the write is current")
                .to_string()
        };
        let first = remember("first", "Deploy notes", "第一版正文");
        let second = remember("second", "Deploy notes", "第二版正文完全不同");
        assert_ne!(first, second, "a second note needs its own id");
        assert_eq!(
            service
                .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
                .unwrap()
                .len(),
            2,
            "neither is lost"
        );
        assert!(
            service
                .read(&external_actor(), ServiceScope::Project, &first)
                .unwrap()
                .body
                .contains("第一版"),
            "the first body survived"
        );

        let a = remember("cjk-a", "状态色约定", "映射表实现");
        let b = remember("cjk-b", "搜索框过滤范围", "只按 payer 过滤");
        assert_ne!(a, b, "same-second CJK titles must not collide");
        assert_eq!(
            service
                .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
                .unwrap()
                .len(),
            4
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A proposal becomes current only when a trusted user accepts it.
    #[test]
    fn user_accept_path_promotes_pending_only_on_explicit_accept() {
        let root =
            std::env::temp_dir().join(format!("leveler-memory-cli-accept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let service = test_service(&root);
        let mut request = RememberRequest::new(
            cli_op_id("pending"),
            ServiceScope::Project,
            "gated memory writes",
            "prefer gated memory writes",
            "preference",
        );
        // An automatic write names the identity it is about.
        request.subject = Some("memory.gated_writes".to_string());
        // The proposal comes from the model path; a local shell process cannot
        // even propose (it could be a model with shell access).
        let model = Actor::Model {
            session: leveler_core::SessionId::new("cli-test-session"),
            turn: leveler_core::TurnId::new("cli-test-turn"),
            tool_call: leveler_core::ToolCallId::new("cli-test-call"),
        };
        let outcome = service.propose(&model, request).unwrap();
        let WriteOutcome::Pending { id, .. } = outcome else {
            panic!("expected pending, got {outcome:?}");
        };
        assert!(
            service
                .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
                .unwrap()
                .is_empty()
        );
        service
            .review(
                &test_user(),
                ServiceScope::Project,
                &id,
                ReviewRequest::Accept,
            )
            .unwrap();
        assert_eq!(
            service
                .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
                .unwrap()
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A plain local process cannot mint user authority: the service refuses
    /// the write outright, and it cannot review anything either.
    #[test]
    fn a_local_cli_write_is_refused_not_user_authorized() {
        let root = std::env::temp_dir().join(format!(
            "leveler-memory-cli-external-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let service = test_service(&root);
        let actor = external_actor();
        let request = RememberRequest::new(
            cli_op_id("remember"),
            ServiceScope::Project,
            "cli note",
            "written by a local process",
            "note",
        );
        let outcome = service.propose(&actor, request).unwrap();
        assert!(
            matches!(outcome, WriteOutcome::Rejected { .. }),
            "a local process is an external caller: {outcome:?}"
        );
        assert!(
            service
                .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
                .unwrap()
                .is_empty(),
            "a local process must not activate memory"
        );
        assert!(
            service
                .pending(&test_user(), ServiceScope::Project)
                .unwrap()
                .is_empty(),
            "a refused write leaves no proposal behind"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
