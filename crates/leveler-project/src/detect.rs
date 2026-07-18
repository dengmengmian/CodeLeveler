//! Language detection from repository marker files.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A programming language CodeLeveler recognizes. Rust/Go/TypeScript are
/// first-class (built-in verification defaults); the rest are detected for
/// context/planning and verified via `.leveler/config.yaml` (spec §3, §37).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    Go,
    TypeScript,
    JavaScript,
    Python,
    Java,
    Ruby,
    CSharp,
    Cpp,
}

impl Language {
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Go => "go",
            Language::TypeScript => "typescript",
            Language::JavaScript => "javascript",
            Language::Python => "python",
            Language::Java => "java",
            Language::Ruby => "ruby",
            Language::CSharp => "csharp",
            Language::Cpp => "c/c++",
        }
    }
}

/// Detect languages at `root` from marker files. Deterministic order.
pub fn detect_languages(root: &Path) -> Vec<Language> {
    let mut out = Vec::new();
    let has = |name: &str| root.join(name).is_file();
    let any_ext = |ext: &str| dir_has_extension(root, ext);

    if has("Cargo.toml") {
        out.push(Language::Rust);
    }
    if has("go.mod") {
        out.push(Language::Go);
    }
    if has("tsconfig.json") {
        out.push(Language::TypeScript);
    } else if has("package.json") {
        // package.json without tsconfig → JavaScript.
        out.push(Language::JavaScript);
    }
    if has("pyproject.toml") || has("setup.py") || has("setup.cfg") || has("requirements.txt") {
        out.push(Language::Python);
    }
    if has("pom.xml") || has("build.gradle") || has("build.gradle.kts") {
        out.push(Language::Java);
    }
    if has("Gemfile") {
        out.push(Language::Ruby);
    }
    if any_ext("csproj") || any_ext("sln") {
        out.push(Language::CSharp);
    }
    if has("CMakeLists.txt") {
        out.push(Language::Cpp);
    }
    out
}

/// Whether any entry directly under `dir` has the given extension.
fn dir_has_extension(dir: &Path, ext: &str) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some(ext))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("leveler-detect-{}", leveler_test_ordinal()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    // A tiny monotonic-ish suffix without pulling in rand or time (both banned in
    // some contexts): use an atomic counter.
    fn leveler_test_ordinal() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn detects_rust() {
        let dir = tmpdir();
        fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_languages(&dir), vec![Language::Rust]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_multiple_in_order() {
        let dir = tmpdir();
        fs::write(dir.join("Cargo.toml"), "").unwrap();
        fs::write(dir.join("go.mod"), "").unwrap();
        fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(
            detect_languages(&dir),
            vec![Language::Rust, Language::Go, Language::TypeScript]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn package_json_without_tsconfig_is_javascript() {
        let dir = tmpdir();
        fs::write(dir.join("package.json"), "{}").unwrap();
        assert_eq!(detect_languages(&dir), vec![Language::JavaScript]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_python_java_csharp() {
        let dir = tmpdir();
        fs::write(dir.join("pyproject.toml"), "").unwrap();
        fs::write(dir.join("pom.xml"), "").unwrap();
        fs::write(dir.join("App.csproj"), "").unwrap();
        let langs = detect_languages(&dir);
        assert!(langs.contains(&Language::Python));
        assert!(langs.contains(&Language::Java));
        assert!(langs.contains(&Language::CSharp));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_when_no_markers() {
        let dir = tmpdir();
        assert!(detect_languages(&dir).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn language_as_str_matches_serde_names() {
        assert_eq!(Language::Rust.as_str(), "rust");
        assert_eq!(Language::Go.as_str(), "go");
        assert_eq!(Language::TypeScript.as_str(), "typescript");
        assert_eq!(Language::JavaScript.as_str(), "javascript");
        assert_eq!(Language::Python.as_str(), "python");
        assert_eq!(Language::Java.as_str(), "java");
        assert_eq!(Language::Ruby.as_str(), "ruby");
        assert_eq!(Language::CSharp.as_str(), "csharp");
        assert_eq!(Language::Cpp.as_str(), "c/c++");
    }

    #[test]
    fn dir_has_extension_finds_matching_files() {
        let dir = tmpdir();
        fs::write(dir.join("App.csproj"), "").unwrap();
        assert!(dir_has_extension(&dir, "csproj"));
        assert!(!dir_has_extension(&dir, "sln"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dir_has_extension_false_for_missing_dir() {
        assert!(!dir_has_extension(Path::new("/does/not/exist"), "csproj"));
    }
}
