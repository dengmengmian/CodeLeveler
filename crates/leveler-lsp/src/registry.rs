//! Known language servers per language.

use leveler_project::Language;

/// How to launch a language server for a language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub program: String,
    pub args: Vec<String>,
    /// The LSP `languageId` for opened documents.
    pub language_id: String,
}

impl ServerSpec {
    fn new(program: &str, args: &[&str], language_id: &str) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            language_id: language_id.to_string(),
        }
    }
}

/// The default language server for a language, if one is known.
pub fn server_for(language: Language) -> Option<ServerSpec> {
    Some(match language {
        Language::Rust => ServerSpec::new(&resolve_rust_analyzer(), &[], "rust"),
        Language::Go => ServerSpec::new("gopls", &[], "go"),
        Language::Python => ServerSpec::new("pylsp", &[], "python"),
        Language::TypeScript => {
            ServerSpec::new("typescript-language-server", &["--stdio"], "typescript")
        }
        Language::JavaScript => {
            ServerSpec::new("typescript-language-server", &["--stdio"], "javascript")
        }
        // No default server wired for these yet.
        Language::Java | Language::Ruby | Language::CSharp | Language::Cpp => return None,
    })
}

/// Whether the server program for a language is installed on PATH.
pub fn server_available(language: Language) -> bool {
    server_available_with_environment(language, leveler_core::environment())
}

pub fn server_available_with_environment(
    language: Language,
    environment: &leveler_core::EnvSnapshot,
) -> bool {
    server_for(language)
        .map(|s| which(&s.program, environment).is_some())
        .unwrap_or(false)
}

/// Resolve `rust-analyzer` to the toolchain binary. The `~/.cargo/bin` entry is
/// a rustup proxy that exits immediately in stdio (LSP) mode, so prefer the real
/// binary under the sysroot; fall back to the PATH name.
fn resolve_rust_analyzer() -> String {
    let mut command = std::process::Command::new("rustc");
    command.args(["--print", "sysroot"]);
    command.env_clear();
    for (name, value) in leveler_core::environment().vars_os() {
        if !name
            .to_str()
            .is_some_and(leveler_execution::is_credential_env_name)
        {
            command.env(name, value);
        }
    }
    if let Ok(out) = command.output()
        && out.status.success()
    {
        let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let candidate = std::path::Path::new(&sysroot)
            .join("bin")
            .join("rust-analyzer");
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "rust-analyzer".to_string()
}

fn which(program: &str, environment: &leveler_core::EnvSnapshot) -> Option<std::path::PathBuf> {
    // An absolute/explicit path is used directly.
    if program.contains('/') {
        let p = std::path::PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    let path = environment.var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_languages_to_servers() {
        assert!(
            server_for(Language::Rust)
                .unwrap()
                .program
                .contains("rust-analyzer")
        );
        assert_eq!(server_for(Language::Go).unwrap().program, "gopls");
        assert!(server_for(Language::Cpp).is_none());
    }
}
