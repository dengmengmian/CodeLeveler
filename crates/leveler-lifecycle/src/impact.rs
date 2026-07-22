//! Unified change-impact view (ChangeImpact) — the single source of truth for
//! "did this change need verification?".
//!
//! Both the readiness gate (`crate::readiness`) and the verification plan
//! scoping (`leveler-verifier`) derive their answer from the same
//! [`is_build_relevant`] heuristic, so they can no longer disagree about
//! whether an inert change (docs, scripts, lock files) demands a fresh verify.

use crate::ledger::EvidenceLedger;

/// What a turn actually touched, and whether it is evidence-backed yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeImpact {
    /// Workspace-relative paths recorded by the ledger's mutations.
    pub modified_files: Vec<String>,
    /// Whether any workspace mutation was recorded at all.
    pub has_mutation: bool,
    /// A successful verify ran after the latest mutation.
    pub verified_after_last_mutation: bool,
    /// Any modified file could affect a build/test outcome — or the blast
    /// radius is unknown (no recorded paths), which is conservatively
    /// treated as relevant.
    pub build_relevant: bool,
}

impl ChangeImpact {
    /// Derive the impact from the evidence ledger's mutation records.
    pub fn from_ledger(ledger: &EvidenceLedger) -> Self {
        let mut modified_files: Vec<String> = Vec::new();
        for mutation in &ledger.mutations {
            for path in &mutation.paths {
                if !modified_files.contains(path) {
                    modified_files.push(path.clone());
                }
            }
        }
        Self {
            has_mutation: ledger.last_mutation_seq() > 0,
            verified_after_last_mutation: ledger.has_fresh_successful_verify(),
            build_relevant: modified_files.is_empty()
                || modified_files.iter().any(|f| is_build_relevant(f)),
            modified_files,
        }
    }
}

/// Whether a modified path could affect a build or test outcome. Conservative:
/// unknown files outside a source tree are treated as inert (return `false`).
/// The trade-off: a whole-workspace `cargo test` (etc.) is neither run nor
/// blamed for its pre-existing red on a change that only touched docs, scripts,
/// lock files, or other non-compiled inputs — but the gates stand unless EVERY
/// modified file is provably inert:
/// - Anything inside a compiled/consumed source tree (`src/`, `tests/`,
///   `benches/`, `examples/`) counts: a fixture there could be an
///   `include_str!` input.
/// - Any known source / manifest / build-driver file (by name or extension,
///   across the ecosystems we gate) counts.
///
/// Known blind spot: a repo whose tests read a tracked top-level `*.sh` or
/// `include_str!` a `.md` living outside a source dir would have that
/// dependency treated as inert. Fixtures under `src/`/`tests/` are covered.
pub fn is_build_relevant(path: &str) -> bool {
    let norm = path.trim_start_matches("./");

    // Inside a compiled/consumed source tree → may be an include!/fixture input.
    for seg in ["src", "tests", "test", "benches", "examples"] {
        if norm == seg || norm.starts_with(&format!("{seg}/")) || norm.contains(&format!("/{seg}/"))
        {
            return true;
        }
    }

    let base = norm.rsplit('/').next().unwrap_or(norm);

    // Manifests and build drivers across the ecosystems whose default gates we
    // emit (Rust/Go/Python/Node) plus common native ones.
    const NAMES: &[&str] = &[
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "go.mod",
        "go.sum",
        "package.json",
        "tsconfig.json",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "Makefile",
        "makefile",
        "CMakeLists.txt",
        "Dockerfile",
    ];
    if NAMES.contains(&base) {
        return true;
    }
    if base.starts_with("requirements") && base.ends_with(".txt") {
        return true;
    }

    // Source and build-config extensions. Docs (.md/.txt/.rst), shell scripts,
    // and lock/metadata files deliberately fall through to inert.
    const EXTS: &[&str] = &[
        "rs", "go", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "c", "cc", "cpp", "cxx", "h",
        "hpp", "java", "rb", "cs", "vue", "svelte", "toml", "json",
    ];
    match base.rsplit_once('.') {
        Some((_, ext)) => EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger_with(paths: &[&str]) -> EvidenceLedger {
        let mut ledger = EvidenceLedger::default();
        for (i, path) in paths.iter().enumerate() {
            ledger.record_mutation(format!("m{i}"), "apply_patch", vec![path.to_string()]);
        }
        ledger
    }

    #[test]
    fn docs_and_assets_only_are_not_build_relevant() {
        let impact = ChangeImpact::from_ledger(&ledger_with(&[
            "README.md",
            "docs/notes.md",
            "assets/logo.png",
            "scripts/audit.sh",
        ]));
        assert!(impact.has_mutation);
        assert!(!impact.build_relevant);
        assert_eq!(impact.modified_files.len(), 4);
    }

    #[test]
    fn source_or_manifest_files_are_build_relevant() {
        let impact = ChangeImpact::from_ledger(&ledger_with(&["crates/x/src/lib.rs"]));
        assert!(impact.build_relevant);

        let impact = ChangeImpact::from_ledger(&ledger_with(&["Cargo.toml"]));
        assert!(impact.build_relevant);
    }

    #[test]
    fn one_relevant_file_makes_the_whole_change_relevant() {
        let impact =
            ChangeImpact::from_ledger(&ledger_with(&["README.md", "crates/x/src/main.rs"]));
        assert!(impact.build_relevant);
    }

    #[test]
    fn unknown_blast_radius_is_conservatively_relevant() {
        // Mutations recorded without paths cannot be proven inert.
        let mut ledger = EvidenceLedger::default();
        ledger.record_mutation("m0", "apply_patch", vec![]);
        let impact = ChangeImpact::from_ledger(&ledger);
        assert!(impact.build_relevant);
    }

    #[test]
    fn freshness_mirrors_the_ledger() {
        let mut ledger = ledger_with(&["src/lib.rs"]);
        let impact = ChangeImpact::from_ledger(&ledger);
        assert!(!impact.verified_after_last_mutation);
        ledger.record_verify("v1", "cargo\u{1f}test", 0);
        let impact = ChangeImpact::from_ledger(&ledger);
        assert!(impact.verified_after_last_mutation);
    }
}
