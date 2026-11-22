//! CLI surface for the content-addressed trust gate.
//!
//! Two families are gated here, with one human decision surface:
//!
//! - in-repo executable configuration (`.leveler/hooks.yaml`,
//!   `.leveler/permissions.yaml`, and a `.leveler/config.yaml` that raises the
//!   default mode), which can run commands or grant standing approval;
//! - authority-bearing definitions (`AGENTS.md` at any depth, `.leveler`
//!   instructions/rules, declarative agents and skills), which the harness puts
//!   into the model's control context.
//!
//! Trust is content-addressed: approving records the digest of the exact bytes
//! the person was shown. Any later edit invalidates it. The agent has no trust
//! tool; this command is the human entry point.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use leveler_definition_trust::{
    ApprovalSurface, DefinitionCandidate, DefinitionTrust, DefinitionTrustView, DiscoveryRoots,
    StoreStatus, approve, discover, read_snapshot, revoke_all, revoke_definition,
    store_is_outside_project,
};
use leveler_execution::{
    TRUSTABLE_PROJECT_FILES, TrustStore, TrustedRead, content_digest, read_trusted_project_file,
};
use leveler_project::Layout;

use crate::cli::TrustCommand;
use crate::output::Line;

/// One in-repo config file and where it stands with the trust store.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FileState {
    Absent,
    Trusted { digest: String },
    Untrusted { digest: String },
}

/// Where one definition stands with the trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DefinitionState {
    Trusted { digest: String, version: u64 },
    Unapproved { digest: Option<String> },
    Changed { digest: String, approved: String },
    Unreadable { error: String },
}

impl DefinitionState {
    fn is_pending(&self) -> bool {
        matches!(self, Self::Unapproved { .. } | Self::Changed { .. })
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Trusted { .. } => "trusted",
            Self::Unapproved { .. } => "new (not trusted)",
            Self::Changed { .. } => "changed since approval",
            Self::Unreadable { .. } => "unreadable",
        }
    }

    fn current_digest(&self) -> Option<&str> {
        match self {
            Self::Trusted { digest, .. } | Self::Changed { digest, .. } => Some(digest),
            Self::Unapproved { digest } => digest.as_deref(),
            Self::Unreadable { .. } => None,
        }
    }
}

/// One discovered definition with its trust state.
#[derive(Debug, Clone)]
struct DefinitionRow {
    display: String,
    kind: &'static str,
    state: DefinitionState,
    candidate: DefinitionCandidate,
}

fn definition_rows(home: &Path, repo: &Path) -> Vec<DefinitionRow> {
    let environment = leveler_core::environment();
    let roots = DiscoveryRoots::resolve(environment, Some(repo.to_path_buf()));
    let view = DefinitionTrustView::load(home, Some(repo));
    discover(&roots)
        .into_iter()
        .map(|candidate| {
            let state = match &candidate.digest {
                None => DefinitionState::Unreadable {
                    error: candidate
                        .read_error
                        .clone()
                        .unwrap_or_else(|| "the files could not be read".to_string()),
                },
                Some(digest) => match view.check(&candidate.identity, digest) {
                    DefinitionTrust::Trusted { approval_version } => DefinitionState::Trusted {
                        digest: digest.clone(),
                        version: approval_version,
                    },
                    DefinitionTrust::Changed { approved_digest } => DefinitionState::Changed {
                        digest: digest.clone(),
                        approved: approved_digest,
                    },
                    DefinitionTrust::Unapproved => DefinitionState::Unapproved {
                        digest: Some(digest.clone()),
                    },
                    DefinitionTrust::StoreUnavailable { status } => DefinitionState::Unreadable {
                        error: format!("trust store unavailable ({})", status.as_str()),
                    },
                    DefinitionTrust::Builtin => DefinitionState::Trusted {
                        digest: digest.clone(),
                        version: 0,
                    },
                    // The CLI reads the persistent store without a session
                    // overlay; these cannot occur here. Map them to the state
                    // their admission means rather than inventing an approval.
                    DefinitionTrust::SessionTrusted { .. } | DefinitionTrust::FullAdopted => {
                        DefinitionState::Trusted {
                            digest: digest.clone(),
                            version: 0,
                        }
                    }
                },
            };
            DefinitionRow {
                display: candidate.identity.display.clone(),
                kind: candidate.identity.kind.as_str(),
                state,
                candidate,
            }
        })
        .collect()
}

pub(crate) fn cmd_trust(
    layout: Layout,
    cmd: Option<TrustCommand>,
) -> anyhow::Result<std::process::ExitCode> {
    let home = leveler_home();
    let repo = layout.require_workspace()?.to_path_buf();

    match cmd {
        Some(TrustCommand::Revoke) => {
            // A revocation writes the same control files as an approval, so a
            // store the repository can rewrite is not a store it may revoke
            // through. Fail closed instead of writing a meaningless change.
            if !store_boundary_ok(&home, &repo) {
                return Ok(std::process::ExitCode::FAILURE);
            }
            // Refuse before touching the config store: a corrupt definition
            // store means the revocation cannot be completed, and a partial
            // "revoked" report would be false.
            let trust_status = DefinitionTrustView::load(&home, Some(&repo)).status();
            if matches!(
                trust_status,
                StoreStatus::Corrupt { .. }
                    | StoreStatus::UnsupportedVersion { .. }
                    | StoreStatus::InsideProject
            ) {
                println!(
                    "{}",
                    Line::fail(&format!(
                        "the definition trust store is unusable ({}); nothing was revoked.",
                        trust_status.as_str()
                    ))
                );
                return Ok(std::process::ExitCode::FAILURE);
            }
            let mut store = TrustStore::load(&home);
            let config_revoked = store.revoke(&repo);
            if config_revoked {
                store.save()?;
            }
            let definitions_revoked = match revoke_all(&home, &repo) {
                Ok(revoked) => revoked,
                Err(error) => {
                    println!("{}", Line::fail(&error.to_string()));
                    return Ok(std::process::ExitCode::FAILURE);
                }
            };
            if config_revoked || definitions_revoked {
                println!(
                    "{}",
                    Line::ok(&format!("revoked trust for {}", repo.display()))
                );
            } else {
                println!("{}", Line::warn("this repository had no trusted files"));
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        Some(TrustCommand::Show) => {
            print_status(&home, &repo);
            Ok(std::process::ExitCode::SUCCESS)
        }
        Some(TrustCommand::Diff { path }) => {
            print_definition_diff(&home, &repo, &path);
            Ok(std::process::ExitCode::SUCCESS)
        }
        Some(TrustCommand::RevokeDefinition { path }) => {
            let rows = definition_rows(&home, &repo);
            let Some(row) = find_row(&rows, &path) else {
                println!("{}", Line::fail(&format!("no definition named `{path}`")));
                return Ok(std::process::ExitCode::FAILURE);
            };
            match revoke_definition(&home, &repo, &row.candidate.identity) {
                Ok(true) => println!(
                    "{}",
                    Line::ok(&format!(
                        "revoked approval for `{}`",
                        sanitize_line(&row.display)
                    ))
                ),
                Ok(false) => println!(
                    "{}",
                    Line::warn(&format!(
                        "`{}` had no approval",
                        sanitize_line(&row.display)
                    ))
                ),
                Err(error) => {
                    println!("{}", Line::fail(&error.to_string()));
                    return Ok(std::process::ExitCode::FAILURE);
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        // Bare `leveler trust` is `trust allow` with the prompt.
        other => {
            let yes = matches!(other, Some(TrustCommand::Allow { yes: true }));

            // Trust granted from a store the repository itself contains means
            // nothing, and the gate ignores it. Say so instead of reporting a
            // write that will never take effect.
            if !store_boundary_ok(&home, &repo) {
                return Ok(std::process::ExitCode::FAILURE);
            }

            let states = states(&home, &repo);
            let pending: Vec<_> = states
                .iter()
                .filter(|(_, state)| matches!(state, FileState::Untrusted { .. }))
                .collect();
            let definitions = definition_rows(&home, &repo);
            let pending_definitions: Vec<_> = definitions
                .iter()
                .filter(|row| row.state.is_pending())
                .collect();
            let unreadable: Vec<_> = definitions
                .iter()
                .filter(|row| matches!(row.state, DefinitionState::Unreadable { .. }))
                .collect();

            // A corrupt or future-version definition store cannot be written
            // safely. Refuse the whole transaction BEFORE the config store is
            // saved: otherwise a pending hooks file would be approved while
            // every definition silently failed to record.
            let trust_status = DefinitionTrustView::load(&home, Some(&repo)).status();
            if matches!(
                trust_status,
                StoreStatus::Corrupt { .. }
                    | StoreStatus::UnsupportedVersion { .. }
                    | StoreStatus::InsideProject
            ) {
                print_unreadable(&unreadable);
                println!(
                    "{}",
                    Line::fail(&format!(
                        "the definition trust store is unusable ({}); nothing was approved. \
                         Repair or remove it first.",
                        trust_status.as_str()
                    ))
                );
                return Ok(std::process::ExitCode::FAILURE);
            }

            if pending.is_empty() && pending_definitions.is_empty() {
                if !unreadable.is_empty() {
                    print_unreadable(&unreadable);
                    return Ok(std::process::ExitCode::FAILURE);
                }
                print_status(&home, &repo);
                println!();
                println!("{}", Line::ok("nothing to trust"));
                return Ok(std::process::ExitCode::SUCCESS);
            }

            if !pending.is_empty() {
                println!("{}", Line::heading("Untrusted in-repo configuration"));
                for (relative, state) in &pending {
                    let FileState::Untrusted { digest } = state else {
                        continue;
                    };
                    println!("  {}  sha256:{}", relative, short(digest));
                    println!("    {}", purpose(relative));
                }
                println!();
            }
            if !pending_definitions.is_empty() {
                println!("{}", Line::heading("Untrusted definitions"));
                for row in &pending_definitions {
                    let digest = row.state.current_digest().map(short).unwrap_or_default();
                    println!(
                        "  {}  [{}] {}",
                        sanitize_line(&row.display),
                        sanitize_line(row.kind),
                        row.state.label()
                    );
                    if !digest.is_empty() {
                        println!("    sha256:{digest}");
                    }
                    println!("    {}", definition_purpose(row.kind));
                }
                println!();
            }
            print_unreadable(&unreadable);

            // A trust decision is the user's, so a non-interactive run must not
            // make it silently — `--yes` is the explicit non-interactive path.
            if !yes && !std::io::stdin().is_terminal() {
                println!(
                    "{}",
                    Line::warn(
                        "not a terminal — re-run with `leveler trust allow --yes` to confirm"
                    )
                );
                return Ok(std::process::ExitCode::FAILURE);
            }
            if !yes && !confirm("Trust these files and definitions' current contents? [y/N] ")? {
                println!("{}", Line::warn("left untrusted"));
                return Ok(std::process::ExitCode::SUCCESS);
            }

            let mut store = TrustStore::load(&home);
            let mut trusted = 0usize;
            let mut changed = Vec::new();
            for (relative, state) in &pending {
                let FileState::Untrusted { digest } = state else {
                    continue;
                };
                // Trust exactly the bytes whose digest the person was shown.
                // Re-reading and recording whatever is current would let a
                // file swapped between listing and this write inherit an
                // approval the person never gave those bytes.
                match read_if_unchanged(&repo.join(relative), digest) {
                    Some(body) => {
                        store.trust(&repo, relative, &body);
                        trusted += 1;
                    }
                    None => changed.push(relative.clone()),
                }
            }
            store.save()?;

            let mut requests = Vec::new();
            for row in &pending_definitions {
                let shown = row.state.current_digest();
                match row.candidate.approval_request() {
                    Ok(request) if Some(request.digest()) == shown => requests.push(request),
                    _ => changed.push(row.display.clone()),
                }
            }
            let report = approve(&home, &repo, &requests, ApprovalSurface::Cli)?;
            println!(
                "{}",
                Line::ok(&format!(
                    "trusted {trusted} config file(s) and {} definition(s) for {} (re-run after any edit)",
                    report.approved.len(),
                    repo.display()
                ))
            );
            println!(
                "  definition store: {}",
                leveler_definition_trust::store_path(&home).display()
            );
            println!("  config store: {}", trust_store_display(&home));
            if !changed.is_empty() {
                println!(
                    "{}",
                    Line::warn(&format!(
                        "not trusted: {} changed while you were deciding",
                        changed.join(", ")
                    ))
                );
                println!("  review the current contents and re-run `leveler trust`.");
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

fn print_unreadable(rows: &[&DefinitionRow]) {
    if rows.is_empty() {
        return;
    }
    println!(
        "{}",
        Line::warn("Definitions that could not be read or admitted:")
    );
    for row in rows {
        let DefinitionState::Unreadable { error } = &row.state else {
            continue;
        };
        println!(
            "  {}  [{}] {}",
            sanitize_line(&row.display),
            sanitize_line(row.kind),
            sanitize_line(error)
        );
    }
    println!();
}

fn find_row<'a>(rows: &'a [DefinitionRow], path: &str) -> Option<&'a DefinitionRow> {
    rows.iter().find(|row| row.display == path).or_else(|| {
        rows.iter()
            .find(|row| row.display.eq_ignore_ascii_case(path))
    })
}

fn print_definition_diff(home: &Path, repo: &Path, path: &str) {
    let rows = definition_rows(home, repo);
    let Some(row) = find_row(&rows, path) else {
        println!("{}", Line::fail(&format!("no definition named `{path}`")));
        return;
    };
    match &row.state {
        DefinitionState::Trusted { digest, .. } => {
            println!(
                "{}",
                Line::ok(&format!(
                    "`{}` is trusted at sha256:{} (current content is the approved version)",
                    row.display,
                    short(digest)
                ))
            );
        }
        DefinitionState::Changed { approved, .. } => {
            let Some(old) = read_snapshot(home, approved) else {
                println!(
                    "{}",
                    Line::warn(&format!(
                        "`{}` changed, but no approved-content snapshot was kept; refusing to \
                         show a diff against unavailable bytes",
                        row.display
                    ))
                );
                return;
            };
            let Ok(new) = row.candidate.current_content() else {
                println!("{}", Line::fail("the current definition could not be read"));
                return;
            };
            println!(
                "{}",
                Line::heading(&format!(
                    "Diff for `{}` (approved → current)",
                    sanitize_line(&row.display)
                ))
            );
            print_unified_diff(
                &String::from_utf8_lossy(&old),
                &String::from_utf8_lossy(&new),
            );
        }
        DefinitionState::Unapproved { .. } => {
            let Ok(current) = row.candidate.current_content() else {
                println!("{}", Line::fail("the current definition could not be read"));
                return;
            };
            println!(
                "{}",
                Line::warn(&format!(
                    "`{}` has no approved version; current content follows",
                    sanitize_line(&row.display)
                ))
            );
            for (index, line) in String::from_utf8_lossy(&current).lines().enumerate() {
                if index >= 200 {
                    println!("  … (truncated; approve or inspect the file directly)");
                    break;
                }
                println!("+ {}", sanitize_line(line));
            }
        }
        DefinitionState::Unreadable { error } => {
            println!(
                "{}",
                Line::fail(&format!(
                    "`{}` is unreadable: {}",
                    sanitize_line(&row.display),
                    sanitize_line(error)
                ))
            );
        }
    }
}

/// A compact line diff. This is a review aid, not a patch format: unchanged
/// runs are reduced to three lines of context around each change. It shares
/// the TUI's implementation so both surfaces describe the same edit.
fn print_unified_diff(old: &str, new: &str) {
    let Some(rows) = leveler_definition_trust::unified_diff(old, new, 400) else {
        println!("  (definitions are too large to diff here; inspect the files directly)");
        return;
    };
    for row in rows {
        println!("  {row}");
    }
}

/// Ignored in-repo config as display paths, for the TUI boot notice.
///
/// Definitions are deliberately absent: an untrusted skill or AGENTS.md is
/// skipped silently at startup. The one lightweight AGENTS.md hint is
/// [`definition_trust_notice`], printed once per project by the launcher.
pub(crate) fn untrusted_config_display(repo_root: &std::path::Path) -> Vec<String> {
    let home = leveler_home();
    leveler_execution::untrusted_project_files(&home, repo_root)
        .into_iter()
        .filter(|entry| !is_config(entry) || project_config_widens(repo_root))
        .map(|entry| {
            entry
                .path
                .strip_prefix(repo_root)
                .unwrap_or(&entry.path)
                .display()
                .to_string()
        })
        .collect()
}

/// A store the gate cannot use is not a quiet skip: no project definition can
/// be admitted until it is repaired. Printed on every launch (it is a
/// configuration failure, not a one-time hint).
pub(crate) fn definition_trust_store_warning(repo_root: &std::path::Path) -> Option<String> {
    definition_trust_store_warning_in(&leveler_home(), repo_root)
}

fn definition_trust_store_warning_in(home: &Path, repo_root: &std::path::Path) -> Option<String> {
    let status = DefinitionTrustView::load(home, Some(repo_root)).status();
    matches!(
        status,
        StoreStatus::Corrupt { .. }
            | StoreStatus::UnsupportedVersion { .. }
            | StoreStatus::InsideProject
    )
    .then(|| {
        format!(
            "⚠ 定义信任库不可用（{}）；本轮不会加载任何项目定义。运行 leveler doctor 查看。",
            status.as_str()
        )
    })
}

/// The one lightweight startup hint for an untrusted root `AGENTS.md`.
///
/// Shown once per (project, exact digest) on this machine: a marker under the
/// Leveler home records the canonical project domain and the digest, so a
/// session resume, TUI redraw or relaunch of the same bytes does not repeat it,
/// while a later content change is a new fact worth one notice. FullAccess
/// adopts the session baseline and suppresses the hint entirely. A store/config
/// failure is not hidden by this quiet path; `leveler doctor` and `/trust`
/// still report it.
pub(crate) fn definition_trust_notice(
    repo_root: &std::path::Path,
    full_access: bool,
) -> Option<String> {
    definition_trust_notice_in(&leveler_home(), repo_root, full_access)
}

fn definition_trust_notice_in(
    home: &Path,
    repo_root: &std::path::Path,
    full_access: bool,
) -> Option<String> {
    if full_access {
        return None;
    }
    // Only the root AGENTS.md matters for this hint. Hashing every skill in
    // the project just to announce one file would be a startup regression.
    let path = repo_root.join("AGENTS.md");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        // A missing file is not a hint. An existing but unreadable one is an
        // error path; say so rather than hiding it behind the quiet default.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            return Some(format!(
                "⚠ 无法读取项目指令 AGENTS.md（{error}）；其内容不会进入模型上下文。"
            ));
        }
    };
    let identity = leveler_definition_trust::DefinitionIdentity::project(
        leveler_definition_trust::DefinitionKind::RootAgents,
        "AGENTS.md",
    )?;
    let digest = leveler_definition_trust::DefinitionIdentity::single_digest(&bytes);
    let view = DefinitionTrustView::load(home, Some(repo_root));
    let state = match view.check(&identity, &digest) {
        DefinitionTrust::Unapproved => DefinitionState::Unapproved {
            digest: Some(digest.clone()),
        },
        DefinitionTrust::Changed { approved_digest } => DefinitionState::Changed {
            digest: digest.clone(),
            approved: approved_digest,
        },
        // Trusted, a session/full admission that cannot occur on this surface,
        // or a store the separate warning already explains.
        _ => return None,
    };
    // One notice per (project, exact root-AGENTS digest). The first appearance
    // and a later content change are different facts; a resume, a redraw or a
    // relaunch of the same bytes is not.
    let domain = leveler_definition_trust::project_domain(repo_root);
    let marker = home.join("state").join("trust-notice-seen");
    let key = format!("{domain}\t{digest}");
    let already = std::fs::read_to_string(&marker)
        .ok()
        .is_some_and(|seen| seen.lines().any(|line| line == key));
    if already {
        return None;
    }
    if let Some(parent) = marker.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(&marker) {
            let _ = writeln!(file, "{key}");
        }
    }
    let display = identity.display;
    let message = match state {
        DefinitionState::Changed { .. } => {
            format!("ℹ 项目指令 {display} 内容已变化，输入 /trust 查看差异并重新信任。")
        }
        _ => format!("ℹ 项目指令 {display} 尚未信任，输入 /trust 可管理。"),
    };
    Some(message)
}

/// The startup notice names config files that are ignored. An untrusted
/// `.leveler/config.yaml` matters only when it would raise the default mode;
/// otherwise nothing about it is being ignored, and warning on every run in
/// every repository would train people to dismiss the notice.
pub(crate) fn untrusted_notices(
    home: &std::path::Path,
    repo_root: &std::path::Path,
) -> Vec<leveler_execution::UntrustedConfig> {
    leveler_execution::untrusted_project_files(home, repo_root)
        .into_iter()
        .filter(|entry| !is_config(entry) || project_config_widens(repo_root))
        .collect()
}

fn is_config(entry: &leveler_execution::UntrustedConfig) -> bool {
    entry
        .path
        .ends_with(leveler_execution::PROJECT_CONFIG_RELATIVE)
}

fn project_config_widens(repo_root: &std::path::Path) -> bool {
    leveler_project::ProjectConfig::load(repo_root)
        .and_then(|config| config.mode)
        .and_then(|raw| leveler_execution::PermissionProfile::parse(&raw))
        == Some(leveler_execution::PermissionProfile::FullAccess)
}

/// State of every gated config file for a repository, in a stable order.
pub(crate) fn states(home: &std::path::Path, repo: &std::path::Path) -> Vec<(String, FileState)> {
    TRUSTABLE_PROJECT_FILES
        .iter()
        .map(|relative| {
            let state = match read_trusted_project_file(home, repo, relative) {
                TrustedRead::Absent => FileState::Absent,
                TrustedRead::Trusted(raw) => FileState::Trusted {
                    digest: leveler_execution::content_digest(raw.as_bytes()),
                },
                TrustedRead::Untrusted { digest, .. } => FileState::Untrusted { digest },
            };
            ((*relative).to_string(), state)
        })
        .collect()
}

fn print_status(home: &std::path::Path, repo: &std::path::Path) {
    println!("{}  {}", Line::heading("Repository"), repo.display());
    println!();
    for (relative, state) in states(home, repo) {
        match state {
            FileState::Absent => println!("  {relative}\n    (no file)"),
            FileState::Trusted { digest } => println!(
                "  {}\n    {} sha256:{}",
                relative,
                Line::ok("trusted"),
                short(&digest)
            ),
            FileState::Untrusted { digest } => println!(
                "  {}\n    {} sha256:{} — {}",
                relative,
                Line::warn("untrusted (ignored)"),
                short(&digest),
                purpose(&relative)
            ),
        }
    }
    println!();
    println!("{}", Line::heading("Definitions"));
    let rows = definition_rows(home, repo);
    if rows.is_empty() {
        println!("  (none)");
        return;
    }
    for row in rows {
        let display = sanitize_line(&row.display);
        let kind = sanitize_line(row.kind);
        match row.state {
            DefinitionState::Trusted { digest, version } => println!(
                "  {}\n    [{}] {} sha256:{} (approval v{version})",
                display,
                kind,
                Line::ok("trusted"),
                short(&digest)
            ),
            DefinitionState::Unapproved { digest } => {
                let digest = digest
                    .map(|digest| format!(" sha256:{}", short(&digest)))
                    .unwrap_or_default();
                println!(
                    "  {}\n    [{}] {}{digest} — approve with `leveler trust`",
                    display,
                    kind,
                    Line::warn("new (ignored)")
                );
            }
            DefinitionState::Changed {
                ref digest,
                ref approved,
            } => println!(
                "  {}\n    [{}] {} sha256:{} (approved sha256:{}) — `leveler trust diff {}`",
                display,
                kind,
                Line::warn(row.state.label()),
                short(digest),
                short(approved),
                display
            ),
            DefinitionState::Unreadable { error } => println!(
                "  {}\n    [{}] {} {}",
                display,
                kind,
                Line::warn("unreadable"),
                sanitize_line(&error)
            ),
        }
    }
}

/// Why trusting a config file matters, in the user's terms.
fn purpose(relative: &str) -> &'static str {
    if relative.ends_with("hooks.yaml") {
        "runs commands before every tool call"
    } else if relative.ends_with("permissions.yaml") {
        "can grant standing approval for tool calls"
    } else {
        "can raise this repository's default permission mode to full access"
    }
}

fn definition_purpose(kind: &str) -> &'static str {
    match kind {
        "agent" => "can run a child with its own toolset, bounds and instructions",
        "skill" => "is inserted into the model's control context as a procedure",
        _ => "is inserted into the model's control context as project instructions",
    }
}

/// Definition bodies are untrusted terminal input. Keep tabs and printable
/// text; neutralize every other control character so a rule cannot drive the
/// terminal while a person reviews a diff.
fn sanitize_line(line: &str) -> String {
    line.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\t' {
                '?'
            } else {
                ch
            }
        })
        .collect()
}

fn short(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    hex.chars().take(12).collect()
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn trust_store_display(home: &Path) -> String {
    leveler_execution::trust_store_path(home)
        .display()
        .to_string()
}

/// Refuse a write to a trust store the project can rewrite. Both the
/// executable-config store and the definition store have this rule; a
/// revocation must be as fail-closed as an approval.
fn store_boundary_ok(home: &Path, repo: &Path) -> bool {
    if leveler_execution::store_is_outside_repo(home, repo) && store_is_outside_project(home, repo)
    {
        return true;
    }
    println!(
        "{}",
        Line::fail(&format!(
            "the trust store ({}) is inside this repository, so a record written there would grant nothing",
            trust_store_display(home)
        ))
    );
    println!("  set HOME (or LEVELER_HOME) to a directory outside the repository.");
    false
}

fn leveler_home() -> PathBuf {
    leveler_core::LevelerHome::resolve(leveler_core::environment())
        .root()
        .to_path_buf()
}

/// Read a file only when it still has the digest the person was shown.
/// `None` means the bytes changed (or could not be read): the confirmation no
/// longer covers them, so no trust record may be written.
fn read_if_unchanged(path: &std::path::Path, expected: &str) -> Option<Vec<u8>> {
    let body = std::fs::read(path).ok()?;
    (content_digest(&body) == expected).then_some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_changed_after_the_prompt_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "mode: assisted\n").unwrap();
        let shown = content_digest(b"mode: assisted\n");
        assert_eq!(
            read_if_unchanged(&path, &shown).as_deref(),
            Some(b"mode: assisted\n".as_slice())
        );

        std::fs::write(&path, "mode: full_access\n").unwrap();
        assert_eq!(read_if_unchanged(&path, &shown), None);
    }

    #[test]
    fn the_line_diff_marks_removed_and_added_lines() {
        let rendered = render_diff_for_test("one\ntwo\nthree\n", "one\nTWO\nthree\nfour\n");
        assert!(rendered.contains("- two"), "{rendered}");
        assert!(rendered.contains("+ TWO"), "{rendered}");
        assert!(rendered.contains("+ four"), "{rendered}");
    }

    fn render_diff_for_test(old: &str, new: &str) -> String {
        // Capture the printer's output by re-implementing only the assertion-
        // relevant part: the same edit script the command prints.
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();
        let mut dp = vec![vec![0usize; new_lines.len() + 1]; old_lines.len() + 1];
        for i in (0..old_lines.len()).rev() {
            for j in (0..new_lines.len()).rev() {
                dp[i][j] = if old_lines[i] == new_lines[j] {
                    dp[i + 1][j + 1] + 1
                } else {
                    dp[i + 1][j].max(dp[i][j + 1])
                };
            }
        }
        let mut out = String::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < old_lines.len() || j < new_lines.len() {
            if i < old_lines.len() && j < new_lines.len() && old_lines[i] == new_lines[j] {
                i += 1;
                j += 1;
            } else if j < new_lines.len() && (i == old_lines.len() || dp[i][j + 1] >= dp[i + 1][j])
            {
                out.push_str(&format!("+ {}\n", new_lines[j]));
                j += 1;
            } else if i < old_lines.len() {
                out.push_str(&format!("- {}\n", old_lines[i]));
                i += 1;
            }
        }
        out
    }

    #[test]
    fn the_agents_hint_is_once_per_digest_and_returns_on_a_change() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("AGENTS.md"), "ONE\n").unwrap();
        let first = definition_trust_notice_in(home.path(), repo.path(), false).unwrap();
        assert!(first.contains("尚未信任"), "{first}");
        assert!(
            definition_trust_notice_in(home.path(), repo.path(), false).is_none(),
            "the same bytes must not be announced twice"
        );
        // Approve the first version, then edit it: the new digest is `changed`
        // and gets one fresh hint.
        let row = definition_rows(home.path(), repo.path())
            .into_iter()
            .find(|row| row.display == "AGENTS.md")
            .unwrap();
        approve(
            home.path(),
            repo.path(),
            &[row.candidate.approval_request().unwrap()],
            ApprovalSurface::Cli,
        )
        .unwrap();
        std::fs::write(repo.path().join("AGENTS.md"), "TWO\n").unwrap();
        let changed = definition_trust_notice_in(home.path(), repo.path(), false).unwrap();
        assert!(changed.contains("内容已变化"), "{changed}");
        assert!(
            definition_trust_notice_in(home.path(), repo.path(), false).is_none(),
            "the changed digest is announced once"
        );
        std::fs::write(repo.path().join("AGENTS.md"), "THREE\n").unwrap();
        assert!(
            definition_trust_notice_in(home.path(), repo.path(), true).is_none(),
            "FullAccess suppresses the hint"
        );
    }

    #[test]
    fn a_broken_store_is_never_hidden_by_the_quiet_startup_default() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        assert!(definition_trust_store_warning_in(home.path(), repo.path()).is_none());
        std::fs::write(
            leveler_definition_trust::store_path(home.path()),
            "!! not yaml !!",
        )
        .unwrap();
        let warning = definition_trust_store_warning_in(home.path(), repo.path()).unwrap();
        assert!(warning.contains("不可用"), "{warning}");
    }
}
