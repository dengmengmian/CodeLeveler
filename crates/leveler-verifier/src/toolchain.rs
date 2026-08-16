//! Toolchain provenance: what the repo declares it needs, and what actually ran.
//!
//! R005-F-P2. F-P1 handled the case where the tool refuses in its own words
//! ("package requires rustc 1.85"). A stale toolchain usually fails in an
//! ordinary way instead — an unsupported syntax error, or Go trying to download
//! the version it was told to use — and that gated as a **code** failure, so the
//! agent was told its code was wrong when its code had never been judged. R005
//! hit it in Rust and R009 in Go; this is the shared shape, not two fixes.
//!
//! Deliberately not a toolchain manager: nothing here installs, pins or
//! switches anything. It reads a declaration, reads a version, and compares.

use std::path::Path;

/// A version requirement the repository states about itself.
pub(crate) struct Declared {
    pub version: String,
    /// The file that says so, named in evidence so the claim is checkable.
    pub source: &'static str,
}

/// The requirement that governs `program`, or `None` when the repo declares
/// none (in which case a failure is the code's, and must stay that way).
pub(crate) fn declared_for(program: &str, repo: &Path) -> Option<Declared> {
    match ecosystem(program)? {
        Ecosystem::Rust => rust_requirement(repo),
        Ecosystem::Go => go_requirement(repo),
    }
}

/// The argument that makes `program` print its version.
pub(crate) fn version_args(program: &str) -> Option<&'static [&'static str]> {
    match ecosystem(program)? {
        Ecosystem::Rust => Some(&["--version"]),
        Ecosystem::Go => Some(&["version"]),
    }
}

/// The version inside `cargo 1.75.0 (…)` / `go version go1.25.8 darwin/arm64`.
pub(crate) fn parse_actual(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|token| token.trim_start_matches("go"))
        .find(|token| {
            let mut parts = token.split('.');
            matches!(parts.next().map(str::parse::<u64>), Some(Ok(_)))
                && matches!(parts.next().map(str::parse::<u64>), Some(Ok(_)))
        })
        .map(ToOwned::to_owned)
}

/// Whether `actual` is strictly older than `required`.
///
/// Unparseable input answers `false`: an environment mismatch must be proven
/// before a code failure is excused as one.
pub(crate) fn is_below(actual: &str, required: &str) -> bool {
    let (Some(actual), Some(required)) = (numeric(actual), numeric(required)) else {
        return false;
    };
    for i in 0..actual.len().max(required.len()) {
        let a = actual.get(i).copied().unwrap_or(0);
        let r = required.get(i).copied().unwrap_or(0);
        if a != r {
            return a < r;
        }
    }
    false
}

enum Ecosystem {
    Rust,
    Go,
}

fn ecosystem(program: &str) -> Option<Ecosystem> {
    match program.rsplit(['/', '\\']).next().unwrap_or(program) {
        "cargo" | "rustc" | "cargo.exe" | "rustc.exe" => Some(Ecosystem::Rust),
        "go" | "go.exe" => Some(Ecosystem::Go),
        _ => None,
    }
}

fn numeric(version: &str) -> Option<Vec<u64>> {
    let trimmed = version.trim().trim_start_matches(['v', 'V']);
    let trimmed = trimmed.trim_start_matches("go");
    // Drop pre-release / build metadata: 1.85.0-nightly compares as 1.85.0.
    let core = trimmed
        .split(['-', '+', ' '])
        .next()
        .unwrap_or(trimmed)
        .trim();
    if core.is_empty() {
        return None;
    }
    core.split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

fn rust_requirement(repo: &Path) -> Option<Declared> {
    // A pinned toolchain is the stronger statement: it says which compiler to
    // use, not merely the oldest one that would do.
    for name in ["rust-toolchain.toml", "rust-toolchain"] {
        if let Ok(text) = std::fs::read_to_string(repo.join(name))
            && let Some(version) = toml_style_value(&text, "channel").or_else(|| {
                // The bare `rust-toolchain` file is just the channel name.
                let bare = text.trim();
                (!bare.is_empty() && !bare.contains('\n')).then(|| bare.to_string())
            })
            && numeric(&version).is_some()
        {
            return Some(Declared {
                version,
                source: if name == "rust-toolchain" {
                    "rust-toolchain"
                } else {
                    "rust-toolchain.toml"
                },
            });
        }
    }
    let text = std::fs::read_to_string(repo.join("Cargo.toml")).ok()?;
    let version = toml_style_value(&text, "rust-version")?;
    Some(Declared {
        version,
        source: "Cargo.toml",
    })
}

fn go_requirement(repo: &Path) -> Option<Declared> {
    let text = std::fs::read_to_string(repo.join("go.mod")).ok()?;
    let mut directive = None;
    for line in text.lines() {
        let line = line.trim();
        // `toolchain go1.25.10` is the exact build the module wants; the `go`
        // directive is the language floor. Prefer the former when present.
        if let Some(rest) = line.strip_prefix("toolchain ") {
            return Some(Declared {
                version: rest.trim().to_string(),
                source: "go.mod",
            });
        }
        if directive.is_none()
            && let Some(rest) = line.strip_prefix("go ")
            && numeric(rest).is_some()
        {
            directive = Some(rest.trim().to_string());
        }
    }
    directive.map(|version| Declared {
        version,
        source: "go.mod",
    })
}

/// `key = "value"` from a flat TOML-ish line, without pulling in a TOML parser
/// for two keys. Ignores commented lines.
fn toml_style_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches(['"', '\'']).trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically_not_lexically() {
        assert!(is_below("1.9.0", "1.10.0"), "1.9 is older than 1.10");
        assert!(is_below("1.25.8", "1.25.10"));
        assert!(!is_below("1.85.0", "1.85"));
        assert!(!is_below("1.86.0", "1.85"));
        // An unparseable version can never prove a mismatch.
        assert!(!is_below("nightly", "1.85"));
        assert!(!is_below("1.85", "stable"));
    }

    #[test]
    fn reads_the_version_each_toolchain_prints() {
        assert_eq!(
            parse_actual("cargo 1.75.0 (1d8b05cdd 2023-11-20)").as_deref(),
            Some("1.75.0")
        );
        assert_eq!(
            parse_actual("go version go1.25.8 darwin/arm64").as_deref(),
            Some("1.25.8")
        );
        assert_eq!(parse_actual("no version here"), None);
    }

    #[test]
    fn a_pinned_toolchain_outranks_the_msrv_floor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nrust-version = \"1.70\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.85.0\"\n",
        )
        .unwrap();
        let declared = declared_for("cargo", dir.path()).unwrap();
        assert_eq!(declared.version, "1.85.0");
        assert_eq!(declared.source, "rust-toolchain.toml");
    }

    #[test]
    fn a_channel_name_is_not_a_version_requirement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nrust-version = \"1.70\"\n",
        )
        .unwrap();
        // "stable" says nothing comparable, so the MSRV floor governs.
        let declared = declared_for("cargo", dir.path()).unwrap();
        assert_eq!(declared.version, "1.70");
        assert_eq!(declared.source, "Cargo.toml");
    }

    #[test]
    fn go_toolchain_line_wins_over_the_language_floor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/x\n\ngo 1.24\n\ntoolchain go1.25.10\n",
        )
        .unwrap();
        let declared = declared_for("go", dir.path()).unwrap();
        assert_eq!(declared.version, "go1.25.10");
    }

    #[test]
    fn a_repo_that_declares_nothing_governs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(declared_for("cargo", dir.path()).is_none());
        assert!(declared_for("make", dir.path()).is_none());
    }
}
