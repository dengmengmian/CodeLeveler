//! Repo-aware verification discovery for ecosystems whose commands live in
//! project manifests (spec §37). Only emits commands that are actually
//! declared by the repository — it never guesses; a repo with no usable
//! manifest yields an empty plan and the run stays `Unverified`.

use std::path::Path;

use leveler_project::{Language, ProjectConfig};

use crate::plan::{CheckKind, VerificationCommand, VerificationPlan};

/// The verification plan for a repository as it exists RIGHT NOW: an explicit
/// `.leveler/config.yaml` verify section if there is one, otherwise whatever the
/// repo's own manifests declare.
///
/// This is deliberately a function of the working tree rather than a value
/// computed once: a turn that creates a project (`go mod init` + tests in a repo
/// that started empty) has nothing to verify when it begins and a full test
/// suite when it ends, and the gate has to see the latter.
pub fn plan_for_repo(root: &Path) -> VerificationPlan {
    let config = ProjectConfig::load(root).unwrap_or_default();
    if !config.verify.is_empty() {
        return plan_from_verify(&config.verify);
    }

    let languages = leveler_project::detect_languages(root);
    let mut plan = VerificationPlan::for_languages(&languages);
    // Node/TS checks live in package.json / tsconfig.json, not in language
    // defaults — read them off the manifests.
    if languages
        .iter()
        .any(|l| matches!(l, Language::TypeScript | Language::JavaScript))
    {
        plan.commands.extend(node_plan(root).commands);
    }
    if !languages.contains(&Language::Rust) {
        plan.commands.extend(nested_rust_plan(root).commands);
    }
    plan
}

/// Build a plan from an explicit `.leveler/config.yaml` verify section (which
/// enables any language or toolchain, including ones we cannot detect).
fn plan_from_verify(spec: &leveler_project::VerifySpec) -> VerificationPlan {
    let mut commands = Vec::new();
    let mut push = |name: &str, cmd: &leveler_project::CommandSpec, kind, gating| {
        commands.push(VerificationCommand {
            name: name.to_string(),
            program: cmd.program.clone(),
            args: cmd.args.clone(),
            kind,
            gating,
            timeout_seconds: 600,
        });
    };
    if let Some(f) = &spec.format {
        push("format", f, CheckKind::Format, false);
    }
    if let Some(b) = &spec.build {
        push("build", b, CheckKind::Build, true);
    }
    if let Some(t) = &spec.test {
        push("test", t, CheckKind::Test, true);
    }
    VerificationPlan { commands }
}

/// Discover Cargo projects one directory below a non-Rust repository root.
/// This covers common monorepo layouts such as `nested-crate/Cargo.toml` without
/// recursively treating every workspace member as a separate verification
/// target.
pub fn nested_rust_plan(root: &Path) -> VerificationPlan {
    let mut manifests = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return VerificationPlan::default();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || matches!(name, "node_modules" | "target") {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let manifest = entry.path().join("Cargo.toml");
        if manifest.is_file() {
            manifests.push(manifest);
        }
    }
    manifests.sort();

    let mut commands = Vec::new();
    for manifest in manifests {
        let relative = manifest.strip_prefix(root).unwrap_or(&manifest);
        let manifest_arg = relative.to_string_lossy().into_owned();
        let label = manifest
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("rust");
        commands.push(VerificationCommand::new(
            &format!("cargo fmt ({label})"),
            "cargo",
            &[
                "fmt",
                "--manifest-path",
                &manifest_arg,
                "--all",
                "--",
                "--check",
            ],
            CheckKind::Format,
            false,
        ));
        commands.push(VerificationCommand::new(
            &format!("cargo check ({label})"),
            "cargo",
            &[
                "check",
                "--manifest-path",
                &manifest_arg,
                "--workspace",
                "--quiet",
            ],
            CheckKind::Build,
            true,
        ));
        commands.push(VerificationCommand::new(
            &format!("cargo test ({label})"),
            "cargo",
            &[
                "test",
                "--manifest-path",
                &manifest_arg,
                "--workspace",
                "--quiet",
            ],
            CheckKind::Test,
            true,
        ));
    }

    VerificationPlan { commands }
}

/// Discover verification commands for a Node/TypeScript repository from
/// `package.json` scripts and `tsconfig.json`. Returns an empty plan when
/// nothing usable is declared.
pub fn node_plan(root: &Path) -> VerificationPlan {
    let pm = package_manager(root);
    let scripts = package_scripts(root);
    let has_script = |name: &str| scripts.iter().any(|s| s == name);

    // Ordered like the language defaults: best-effort lint first, then the
    // gating type/build checks, tests last.
    let mut commands = Vec::new();
    let mut script = |name: &str, kind, gating| {
        if has_script(name) {
            commands.push(VerificationCommand::new(
                name,
                pm,
                &["run", name],
                kind,
                gating,
            ));
        }
    };
    script("lint", CheckKind::Lint, false);
    script("typecheck", CheckKind::Build, true);
    script("build", CheckKind::Build, true);
    script("test", CheckKind::Test, true);

    // A tsconfig without a declared typecheck script still gets a type gate.
    // Prefer the repo-local binary; a bare `tsc` that is not on PATH becomes
    // ToolMissing (unverified) rather than a guessed npx invocation.
    if root.join("tsconfig.json").is_file() && !has_script("typecheck") {
        let local = root.join("node_modules/.bin/tsc");
        let program = if local.is_file() {
            local.to_string_lossy().into_owned()
        } else {
            "tsc".to_string()
        };
        commands.push(VerificationCommand::new(
            "tsc",
            &program,
            &["--noEmit"],
            CheckKind::Build,
            true,
        ));
    }

    VerificationPlan { commands }
}

/// Pick the package manager from the lockfile present at the root.
fn package_manager(root: &Path) -> &'static str {
    if root.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if root.join("yarn.lock").is_file() {
        "yarn"
    } else {
        "npm"
    }
}

/// The names of usable `package.json` scripts (the npm-init placeholder test
/// script is not a real check).
fn package_scripts(root: &Path) -> Vec<String> {
    let path = root.join("package.json");
    if !path.is_file() {
        return Vec::new();
    }
    let parsed: serde_json::Value = match std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("ignoring unreadable {}: {e}", path.display());
            return Vec::new();
        }
    };
    let Some(scripts) = parsed.get("scripts").and_then(|s| s.as_object()) else {
        return Vec::new();
    };
    scripts
        .iter()
        .filter(|(_, v)| {
            !v.as_str()
                .is_some_and(|body| body.contains("no test specified"))
        })
        .map(|(k, _)| k.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, content: &str) {
        std::fs::write(root.join(name), content).unwrap();
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn find<'a>(plan: &'a VerificationPlan, name: &str) -> &'a VerificationCommand {
        plan.commands
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no command named {name} in {:?}", plan.commands))
    }

    #[test]
    fn package_json_scripts_become_checks() {
        let dir = tmp();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts":{"typecheck":"tsc --noEmit","build":"vite build","test":"vitest run","lint":"eslint ."}}"#,
        );
        let plan = node_plan(dir.path());
        assert_eq!(plan.commands.len(), 4);

        let lint = find(&plan, "lint");
        assert_eq!(lint.program, "npm");
        assert_eq!(lint.args, vec!["run", "lint"]);
        assert_eq!(lint.kind, CheckKind::Lint);
        assert!(!lint.gating);

        let typecheck = find(&plan, "typecheck");
        assert_eq!(typecheck.args, vec!["run", "typecheck"]);
        assert_eq!(typecheck.kind, CheckKind::Build);
        assert!(typecheck.gating);

        let build = find(&plan, "build");
        assert_eq!(build.kind, CheckKind::Build);
        assert!(build.gating);

        let test = find(&plan, "test");
        assert_eq!(test.kind, CheckKind::Test);
        assert!(test.gating);
    }

    #[test]
    fn lockfiles_select_package_manager() {
        let dir = tmp();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts":{"test":"vitest"}}"#,
        );
        write(dir.path(), "pnpm-lock.yaml", "");
        assert_eq!(node_plan(dir.path()).commands[0].program, "pnpm");

        let dir = tmp();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts":{"test":"vitest"}}"#,
        );
        write(dir.path(), "yarn.lock", "");
        assert_eq!(node_plan(dir.path()).commands[0].program, "yarn");
    }

    #[test]
    fn npm_default_test_placeholder_is_ignored() {
        let dir = tmp();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        );
        assert!(node_plan(dir.path()).commands.is_empty());
    }

    #[test]
    fn tsconfig_without_typecheck_script_adds_tsc() {
        let dir = tmp();
        write(dir.path(), "tsconfig.json", "{}");
        let plan = node_plan(dir.path());
        let tsc = find(&plan, "tsc");
        assert_eq!(tsc.program, "tsc");
        assert_eq!(tsc.args, vec!["--noEmit"]);
        assert_eq!(tsc.kind, CheckKind::Build);
        assert!(tsc.gating);
    }

    #[test]
    fn tsconfig_prefers_local_tsc_binary() {
        let dir = tmp();
        write(dir.path(), "tsconfig.json", "{}");
        let bin = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("tsc"), "#!/bin/sh\n").unwrap();
        let plan = node_plan(dir.path());
        let tsc = find(&plan, "tsc");
        assert_eq!(tsc.program, bin.join("tsc").to_string_lossy());
    }

    #[test]
    fn typecheck_script_supersedes_tsc_fallback() {
        let dir = tmp();
        write(dir.path(), "tsconfig.json", "{}");
        write(
            dir.path(),
            "package.json",
            r#"{"scripts":{"typecheck":"tsc -b"}}"#,
        );
        let plan = node_plan(dir.path());
        assert_eq!(plan.commands.len(), 1);
        assert_eq!(plan.commands[0].name, "typecheck");
    }

    #[test]
    fn empty_dir_yields_empty_plan() {
        let dir = tmp();
        assert!(node_plan(dir.path()).commands.is_empty());
    }

    #[test]
    fn malformed_package_json_yields_empty_plan() {
        let dir = tmp();
        write(dir.path(), "package.json", "not json");
        assert!(node_plan(dir.path()).commands.is_empty());
    }
}
