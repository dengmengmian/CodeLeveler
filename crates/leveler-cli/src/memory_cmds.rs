//! CLI surface for project-scoped durable memory (WM-04).
//!
//! Every read and write goes through the unified [`MemoryService`], so the
//! backend the project selects (`legacy` JSON or `markdown`) is the only store
//! this command touches.
//!
//! This is a plain local process, so it cannot mint a user actor: a model with
//! shell access can run the same argv. Writes therefore run as
//! [`Actor::External`]; the service may queue them as pending or reject them,
//! but it never turns a CLI argument into user authority. Trusted user review
//! (accept/reject/restore and direct activation) is available only from the
//! TUI/desktop, where the command handler mints `UserInvocationId` in process.
//!
//! `audit`, `audit-identity` and `scrub` are legacy-store maintenance
//! operations with no markdown equivalent; they refuse on a markdown project
//! instead of opening a store the project does not use.

use std::path::Path;

use leveler_core::MemoryBackend;
use leveler_memory::{
    MemoryLanes, MemoryService, MemoryStore, MemoryView, RememberRequest, ReviewRequest, ServiceError,
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
            println!("{}", Line::ok(&format!("[{id}] {title}: {operation}{detail}")));
            std::process::ExitCode::SUCCESS
        }
        WriteOutcome::Pending {
            id,
            title,
            reason,
        } => {
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
            println!("{}", Line::ok(&format!("archived [{id}]: {title}")));
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

/// Legacy-only maintenance commands have no service equivalent; refuse them on
/// a markdown project rather than opening a store that project does not use.
fn legacy_maintenance_store(
    backend: MemoryBackend,
    layout: &Layout,
) -> anyhow::Result<Option<MemoryStore>> {
    if backend != MemoryBackend::Legacy {
        println!(
            "{}",
            Line::warn(
                "this maintenance command only operates on the legacy JSON store; the project uses the markdown backend"
            )
        );
        return Ok(None);
    }
    Ok(Some(MemoryStore::open(&layout.memory_dir())?))
}

pub(crate) fn cmd_memory(
    layout: Layout,
    cmd: MemoryCommand,
) -> anyhow::Result<std::process::ExitCode> {
    // Must run before any store is opened: `MemoryStore::open` creates its
    // root, and inspecting a broken scope is only useful if it changes
    // nothing.
    if let MemoryCommand::Diagnose {
        current,
        state,
        scope,
    } = &cmd
    {
        return cmd_memory_diagnose(&layout, current, state, *scope);
    }

    let config = layout
        .primary_workspace()
        .and_then(leveler_project::ProjectConfig::load)
        .unwrap_or_default();
    let backend = config.memory.backend;
    let lanes = leveler_app::MemoryPolicy::from_project(&config).lanes();
    let service = leveler_app::memory_service(&layout, backend);
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
            match backend {
                MemoryBackend::Legacy => {
                    println!("  backend=legacy memory_dir={}", layout.memory_dir().display());
                }
                MemoryBackend::Markdown => {
                    let current = layout
                        .markdown_memory_dir()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "(no workspace)".to_string());
                    println!("  backend=markdown current_dir={current}");
                }
            }
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
            println!("{}", Line::heading("Memory switches"));
            println!("  total: {}", on(total));
            println!(
                "  user facts: {} (active {} · archived {})",
                on(user),
                active.len().saturating_sub(experience_active),
                archived.len().saturating_sub(experience_archived),
            );
            println!(
                "  auto experience: {}{} (active {} · archived {})",
                on(experience),
                experimental,
                experience_active,
                experience_archived,
            );
            match service.pending(&actor, scope) {
                Ok(pending) => {
                    let experience_pending =
                        pending.iter().filter(|view| view.is_experience()).count();
                    println!(
                        "  pending review: {} (user {} · experience {})",
                        pending.len(),
                        pending.len().saturating_sub(experience_pending),
                        experience_pending,
                    );
                }
                Err(ServiceError::NeedsReview(_)) => {
                    println!("  pending review: only visible from the TUI/desktop")
                }
                Err(error) => return Err(error.into()),
            }
            println!("  backend: {}", backend.as_str());
            Ok(std::process::ExitCode::SUCCESS)
        }
        MemoryCommand::Search { query, limit } => {
            let hits = match service.search(&actor, scope, lanes, &query, limit, false) {
                Ok(hits) => hits,
                Err(ServiceError::Unavailable(reason))
                    if reason == leveler_memory::MARKDOWN_SEARCH_UNAVAILABLE =>
                {
                    println!(
                        "{}",
                        Line::warn(&format!(
                            "search is not available on the markdown backend yet ({reason}); this is not an empty result"
                        ))
                    );
                    return Ok(std::process::ExitCode::FAILURE);
                }
                Err(error) => return Err(error.into()),
            };
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
                        println!(
                            "{}",
                            Line::warn(&format!("not found in archive: {id}"))
                        );
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
                            Line::warn(&format!(
                                "not current: {id}; read it with --archived"
                            ))
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
                    println!("{}", Line::ok(&format!("archived [{id}]: {title}")));
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
        MemoryCommand::Restore { id } => {
            if backend == MemoryBackend::Markdown {
                println!(
                    "{}",
                    Line::warn(
                        "restore is not available on the markdown backend; forget is a tombstone"
                    )
                );
                return Ok(std::process::ExitCode::FAILURE);
            }
            match service.restore(&actor, scope, &id) {
                Ok(outcome) => Ok(report_outcome(outcome)),
                Err(ServiceError::NotFound(_)) => {
                    println!("{}", Line::warn(&format!("not found: {id}")));
                    Ok(std::process::ExitCode::FAILURE)
                }
                Err(ServiceError::NeedsReview(_)) => {
                    println!(
                        "{}",
                        Line::warn(
                            "restore requires the TUI/desktop; a local process cannot claim user authority"
                        )
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
            let mut request = RememberRequest::new(
                cli_op_id("remember"),
                scope,
                title,
                body,
                kind_label(kind),
            );
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
                    println!(
                        "  a local process cannot claim user authority; use the TUI/desktop"
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
        MemoryCommand::Audit => {
            let Some(store) = legacy_maintenance_store(backend, &layout)? else {
                return Ok(std::process::ExitCode::FAILURE);
            };
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
            let Some(store) = legacy_maintenance_store(backend, &layout)? else {
                return Ok(std::process::ExitCode::FAILURE);
            };
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
            let Some(store) = legacy_maintenance_store(backend, &layout)? else {
                return Ok(std::process::ExitCode::FAILURE);
            };
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
        MemoryCommand::Diagnose { .. } => {
            unreachable!("diagnose is handled before any store is opened")
        }
    }
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

/// The legacy store semantics the trusted in-process direct path relies on.
///
/// Uses `activate`, NOT `remember`: the latter is an upsert by id and would
/// silently replace an existing memory whenever a new title slugs to the same
/// id. Kept here as a regression test because the TUI's `/remember` and the
/// executor's direct command both reach the same store through the service.
#[cfg(test)]
fn remember_direct(
    store: &MemoryStore,
    title: &str,
    body: &str,
    kind: crate::cli::MemoryKindArg,
    tags: Vec<String>,
) -> Result<leveler_memory::MemoryEntry, leveler_memory::MemoryError> {
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

    /// User-authoritative accept path used by the TUI (not agent AutoApprove).
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

    /// A plain local process cannot mint user authority: the service queues a
    /// create as pending and refuses review outright.
    #[test]
    fn a_local_cli_write_is_pending_not_user_authorized() {
        let root =
            std::env::temp_dir().join(format!("leveler-memory-cli-external-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let service = MemoryService::legacy(root.as_path());
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
            matches!(outcome, WriteOutcome::Pending { .. }),
            "expected pending, got {outcome:?}"
        );
        let store = MemoryStore::open(&root).unwrap();
        assert_eq!(
            store.list_active().unwrap().len(),
            0,
            "a local process must not activate memory"
        );
        let pending = store.list_pending().unwrap();
        assert_eq!(pending.len(), 1);

        let review = service.review(
            &actor,
            ServiceScope::Project,
            &pending[0].id,
            ReviewRequest::Accept,
        );
        assert!(
            matches!(review, Err(ServiceError::NeedsReview(_))),
            "a local process must not review its own proposal: {review:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
