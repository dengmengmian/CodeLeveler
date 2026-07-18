//! Command-backed acceptance-criteria evidence (spec §29 completion proof).
//!
//! Each acceptance criterion carries a shell check command (its
//! `verification_hint`). Running it turns the criterion into deterministic
//! evidence: exit 0 → Met, non-zero → Unmet, no command / trivial / dangerous →
//! Unverifiable. The ledger is the "did we actually satisfy what was asked"
//! record, distinct from the build/test gate.
//!
//! When understand yields no executable **required** criteria, the engine may
//! fill gaps with [`synthesize_mutation_acceptance`] (delete → `test ! -e`).

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Cap on mutation-derived criteria so a large dirty tree cannot flood the ledger.
pub const MAX_MUTATION_DERIVED_CHECKS: usize = 20;

/// One acceptance criterion to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceCheck {
    pub id: String,
    pub description: String,
    /// The shell command that exits 0 iff the criterion holds. `None`/empty
    /// means the criterion cannot be checked by a command.
    pub command: Option<String>,
    pub required: bool,
}

impl AcceptanceCheck {
    /// Whether this criterion has a non-empty command string (may still be
    /// rejected as trivial/dangerous at evaluate time).
    pub fn has_command(&self) -> bool {
        self.command
            .as_ref()
            .map(|c| !c.trim().is_empty())
            .unwrap_or(false)
    }
}

/// The evaluated status of one criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceStatus {
    /// Its check command ran and passed.
    Met,
    /// Its check command ran and failed.
    Unmet,
    /// No command to check it — deliberately not asserted.
    Unverifiable,
}

/// Evidence for one criterion after evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceEvidence {
    pub id: String,
    pub description: String,
    pub required: bool,
    pub status: AcceptanceStatus,
    /// The command that was run, if any.
    pub command: Option<String>,
    /// Captured output (truncated) — the evidence.
    pub evidence: String,
    /// Why the criterion was not executed / not proven, when applicable:
    /// `no_command`, `trivial`, `dangerous`, `cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
}

/// The full acceptance ledger for a task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceLedger {
    pub items: Vec<AcceptanceEvidence>,
}

impl AcceptanceLedger {
    /// Whether any REQUIRED criterion is provably Unmet (its check command ran
    /// and failed).
    pub fn has_required_unmet(&self) -> bool {
        self.items
            .iter()
            .any(|i| i.required && i.status == AcceptanceStatus::Unmet)
    }

    /// Whether any REQUIRED criterion could not be checked (no command, trivial,
    /// dangerous, cancelled, …). Required Unverifiable is **not** neutral: it
    /// blocks a `Verified` claim (design K2).
    pub fn has_required_unverifiable(&self) -> bool {
        self.items
            .iter()
            .any(|i| i.required && i.status == AcceptanceStatus::Unverifiable)
    }

    /// Every required criterion is `Met`. Empty required set → `true` (nothing
    /// blocks Verified via the "all Met" check alone). Equivalent to
    /// `!has_required_unmet() && !has_required_unverifiable()` over required
    /// items. Implementation-class tasks need the stronger
    /// [`has_proven_required_met`](Self::has_proven_required_met) bar.
    pub fn all_required_met(&self) -> bool {
        self.items
            .iter()
            .filter(|i| i.required)
            .all(|i| i.status == AcceptanceStatus::Met)
    }

    /// At least one **required** criterion is `Met` (command ran and passed).
    /// Empty ledgers, only-optional, or only-Unverifiable required items are
    /// **not** proven. Used for implementation-class Verified (design post
    /// hard-screen: cannot Verified on empty/unproven AC alone).
    pub fn has_proven_required_met(&self) -> bool {
        self.items
            .iter()
            .any(|i| i.required && i.status == AcceptanceStatus::Met)
    }

    /// Required criteria that are provably Unmet.
    pub fn unmet_required(&self) -> Vec<&AcceptanceEvidence> {
        self.items
            .iter()
            .filter(|i| i.required && i.status == AcceptanceStatus::Unmet)
            .collect()
    }
}

/// Whether any criterion is required **and** carries a non-empty command.
pub fn has_executable_required(checks: &[AcceptanceCheck]) -> bool {
    checks.iter().any(|c| c.required && c.has_command())
}

/// Merge understand / fallback criteria with mutation-derived path checks.
///
/// Policy:
/// 1. Understand hints **win** when at least one required criterion already has
///    a non-empty command — no mutation synthesis is added.
/// 2. Otherwise demote required items that lack a command (so empty/fallback AC
///    cannot block via Unverifiable), then append
///    [`synthesize_mutation_acceptance`] for deleted paths.
/// 3. Content-only edits (path still present) do **not** get a required
///    existence check — that would false-Verified content bugs.
pub fn assemble_acceptance_checks(
    mut from_requirement: Vec<AcceptanceCheck>,
    workspace_root: &Path,
    modified_files: &[String],
) -> Vec<AcceptanceCheck> {
    if has_executable_required(&from_requirement) {
        return from_requirement;
    }
    for check in &mut from_requirement {
        if check.required && !check.has_command() {
            check.required = false;
        }
    }
    from_requirement.extend(synthesize_mutation_acceptance(
        workspace_root,
        modified_files,
    ));
    from_requirement
}

/// Derive safe, required path-absence checks for modified files that no longer
/// exist on disk (deletes / moves out of tree).
///
/// Only **missing** paths become required AC (`test ! -e 'rel'`). Paths that
/// still exist are skipped — content correctness cannot be inferred from
/// "file is present".
pub fn synthesize_mutation_acceptance(
    workspace_root: &Path,
    modified_files: &[String],
) -> Vec<AcceptanceCheck> {
    let mut out = Vec::new();
    for raw in modified_files {
        if out.len() >= MAX_MUTATION_DERIVED_CHECKS {
            break;
        }
        let Some(rel) = sanitize_workspace_rel_path(raw) else {
            continue;
        };
        let abs = workspace_root.join(&rel);
        // Only prove deletes: path must not exist in the workspace now.
        if abs.exists() {
            continue;
        }
        let quoted = shell_single_quote(&rel);
        let command = format!("test ! -e {quoted}");
        let id = mutation_delete_id(&rel, out.len());
        out.push(AcceptanceCheck {
            id,
            description: format!("deleted path absent: {rel}"),
            command: Some(command),
            required: true,
        });
    }
    out
}

/// Reject absolute paths, `..`, empty, control chars; return a normalized
/// relative path string using `/` separators.
pub fn sanitize_workspace_rel_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains('\0') || raw.contains('\n') || raw.contains('\r') {
        return None;
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return None;
    }
    // Windows drive / UNC style
    if raw.contains(':') {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(s) => {
                let s = s.to_string_lossy();
                if s.is_empty() || s == "." {
                    continue;
                }
                parts.push(s.into_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn shell_single_quote(s: &str) -> String {
    // POSIX: 'foo'\''bar' for embedded single quotes.
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn mutation_delete_id(rel: &str, index: usize) -> String {
    let mut slug: String = rel
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if slug.len() > 48 {
        slug.truncate(48);
    }
    if slug.is_empty() {
        slug = format!("p{index}");
    }
    format!("MUT-DEL-{slug}")
}

/// Resolve a path under `workspace_root` for existence checks (test helper /
/// callers that need the joined path). Returns `None` if `raw` is unsafe.
pub fn workspace_join_rel(workspace_root: &Path, raw: &str) -> Option<PathBuf> {
    sanitize_workspace_rel_path(raw).map(|rel| workspace_root.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ev(required: bool, status: AcceptanceStatus) -> AcceptanceEvidence {
        AcceptanceEvidence {
            id: "AC".into(),
            description: "d".into(),
            required,
            status,
            command: None,
            evidence: String::new(),
            reject_reason: None,
        }
    }

    #[test]
    fn only_a_required_unmet_triggers_has_required_unmet() {
        // Optional Unmet does not count; required Unverifiable is a different flag.
        let mixed = AcceptanceLedger {
            items: vec![
                ev(true, AcceptanceStatus::Met),
                ev(false, AcceptanceStatus::Unmet), // optional
                ev(true, AcceptanceStatus::Unverifiable), // required but uncheckable
            ],
        };
        assert!(!mixed.has_required_unmet());
        assert!(mixed.has_required_unverifiable());
        assert!(!mixed.all_required_met());
        assert!(mixed.unmet_required().is_empty());

        // A required criterion that provably failed IS the Unmet trigger.
        let failed = AcceptanceLedger {
            items: vec![
                ev(true, AcceptanceStatus::Met),
                ev(true, AcceptanceStatus::Unmet),
            ],
        };
        assert!(failed.has_required_unmet());
        assert!(!failed.has_required_unverifiable());
        assert!(!failed.all_required_met());
        assert_eq!(failed.unmet_required().len(), 1);
    }

    #[test]
    fn required_unverifiable_blocks_all_required_met() {
        // Optional Unverifiable / Unmet must not block Verified.
        let optional_only = AcceptanceLedger {
            items: vec![
                ev(false, AcceptanceStatus::Unverifiable),
                ev(false, AcceptanceStatus::Unmet),
            ],
        };
        assert!(!optional_only.has_required_unmet());
        assert!(!optional_only.has_required_unverifiable());
        assert!(optional_only.all_required_met());

        // Empty ledger: nothing required → Met.
        let empty = AcceptanceLedger::default();
        assert!(empty.all_required_met());
        assert!(!empty.has_required_unmet());
        assert!(!empty.has_required_unverifiable());

        // Required Unverifiable blocks Verified (K2).
        let blocked = AcceptanceLedger {
            items: vec![ev(true, AcceptanceStatus::Unverifiable)],
        };
        assert!(blocked.has_required_unverifiable());
        assert!(!blocked.all_required_met());

        // All required Met → clear.
        let ok = AcceptanceLedger {
            items: vec![
                ev(true, AcceptanceStatus::Met),
                ev(false, AcceptanceStatus::Unverifiable),
            ],
        };
        assert!(ok.all_required_met());
        assert!(ok.has_proven_required_met());
        assert!(!ok.has_required_unmet());
        assert!(!ok.has_required_unverifiable());
    }

    #[test]
    fn has_proven_required_met_requires_a_met_required_item() {
        assert!(!AcceptanceLedger::default().has_proven_required_met());
        assert!(
            !AcceptanceLedger {
                items: vec![ev(false, AcceptanceStatus::Met)],
            }
            .has_proven_required_met()
        );
        assert!(
            !AcceptanceLedger {
                items: vec![ev(true, AcceptanceStatus::Unverifiable)],
            }
            .has_proven_required_met()
        );
        assert!(
            AcceptanceLedger {
                items: vec![ev(true, AcceptanceStatus::Met)],
            }
            .has_proven_required_met()
        );
    }

    #[test]
    fn sanitize_rejects_absolute_parent_and_control() {
        assert!(sanitize_workspace_rel_path("").is_none());
        assert!(sanitize_workspace_rel_path("/etc/passwd").is_none());
        assert!(sanitize_workspace_rel_path("../x").is_none());
        assert!(sanitize_workspace_rel_path("a/../../b").is_none());
        assert!(sanitize_workspace_rel_path("a\nb").is_none());
        assert!(sanitize_workspace_rel_path("C:\\Windows").is_none());
        assert_eq!(
            sanitize_workspace_rel_path("./src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            sanitize_workspace_rel_path("quicksort.py").as_deref(),
            Some("quicksort.py")
        );
    }

    #[test]
    fn synthesize_only_missing_paths_as_required_delete_checks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("keep.rs"), "x").unwrap();
        // deleted.rs never created → missing
        let checks = synthesize_mutation_acceptance(
            dir.path(),
            &["keep.rs".into(), "deleted.rs".into(), "../escape".into()],
        );
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].id, "MUT-DEL-deleted.rs");
        assert!(checks[0].required);
        let cmd = checks[0].command.as_deref().unwrap();
        assert!(cmd.starts_with("test ! -e "), "{cmd}");
        assert!(cmd.contains("'deleted.rs'"), "{cmd}");
        assert!(!is_trivial_like(cmd));
    }

    fn is_trivial_like(cmd: &str) -> bool {
        // Mirror: vacuous true is trivial; path tests are not.
        cmd.trim() == "true" || cmd.trim() == ":"
    }

    #[test]
    fn assemble_skips_mutation_when_understand_has_executable_required() {
        let dir = tempfile::tempdir().unwrap();
        let from = vec![AcceptanceCheck {
            id: "AC-1".into(),
            description: "from model".into(),
            command: Some("grep -q foo src/x.rs".into()),
            required: true,
        }];
        let out = assemble_acceptance_checks(from.clone(), dir.path(), &["gone.py".into()]);
        assert_eq!(out, from, "understand wins — no MUT-DEL appended");
    }

    #[test]
    fn assemble_demotes_empty_required_and_adds_delete_checks() {
        let dir = tempfile::tempdir().unwrap();
        let from = vec![AcceptanceCheck {
            id: "AC-1".into(),
            description: "fallback".into(),
            command: None,
            required: true, // understand-style empty required
        }];
        let out = assemble_acceptance_checks(from, dir.path(), &["gone.py".into()]);
        assert_eq!(out.len(), 2);
        assert!(
            !out[0].required,
            "empty required demoted so it cannot block"
        );
        assert!(out[1].id.starts_with("MUT-DEL-"));
        assert!(out[1].required);
        assert!(out[1].has_command());
    }

    #[test]
    fn assemble_edit_only_without_executable_ac_stays_unproven() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("src.rs"), "x").unwrap();
        let from = vec![AcceptanceCheck {
            id: "AC-1".into(),
            description: "fallback".into(),
            command: None,
            required: false,
        }];
        let out = assemble_acceptance_checks(from, dir.path(), &["src.rs".into()]);
        // File still exists → no MUT-DEL; no executable required remains.
        assert!(!has_executable_required(&out));
    }

    #[test]
    fn shell_quote_embeds_single_quotes_safely() {
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_single_quote("plain"), "'plain'");
    }
}
